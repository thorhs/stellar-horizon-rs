use crate::error::{Error, Result};
use crate::horizon_error::HorizonError;
use crate::request::{Request, StreamRequest};
use futures::future::{BoxFuture, Future};
use futures::stream::{BoxStream, IntoAsyncRead, TryStreamExt};
use futures::Stream;
use hyper::client::ResponseFuture;
use hyper::Client;
use hyper_tls::HttpsConnector;
use std::convert::TryInto;
use std::marker::Unpin;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use url::Url;

/// Horizon Client trait. Send HTTP and stream requests to Horizon.
pub trait HorizonClient {
    /// Send a request `R` to horizon, returns the corresponding response.
    fn request<'a, R: Request + 'a>(&'a self, req: R) -> BoxFuture<'a, Result<R::Response>>;
    /// Create a stream request.
    fn stream<'a, R: StreamRequest + 'a>(
        &'a self,
        req: R,
    ) -> Result<Box<dyn Stream<Item = Result<R::Resource>> + 'a + Unpin>>;
}

type HttpClient = Client<HttpsConnector<hyper::client::HttpConnector>>;

/// Type that implements `HorizonClient` using `hyper` for http.
pub struct HorizonHttpClient {
    inner: HttpClient,
    host: Url,
    client_name: String,
    client_version: String,
}

type BoxDecoder = Box<dyn Unpin + Stream<Item = http_types::Result<async_sse::Event>>>;

/// A `Stream` that represents a horizon stream connection.
#[must_use = "Streams are lazy and do nothing unless polled"]
pub struct HorizonHttpStream<'a, R>
where
    R: StreamRequest,
{
    client: &'a HorizonHttpClient,
    last_id: Option<String>,
    request: R,
    response: Option<ResponseFuture>,
    decoder: Option<BoxDecoder>,
}

impl HorizonHttpClient {
    /// Creates a new horizon client with the specified host url str.
    pub fn new_from_str(host: &str) -> Result<HorizonHttpClient> {
        let host: Url = host.parse().map_err(|_| Error::InvalidHost)?;
        HorizonHttpClient::new(host)
    }

    /// Creates a new horizon client with the specified host url.
    pub fn new<U>(host: U) -> Result<HorizonHttpClient>
    where
        U: TryInto<Url>,
    {
        let https = HttpsConnector::new();
        let inner = Client::builder().build::<_, hyper::Body>(https);
        let host = host.try_into().map_err(|_| Error::InvalidHost)?;
        let client_name = "aurora-rs/stellar-sdk".to_string();
        let client_version = crate::VERSION.to_string();
        Ok(HorizonHttpClient {
            inner,
            host,
            client_name,
            client_version,
        })
    }

    /// Returns a request builder with default headers.
    fn request_builder(&self, uri: Url) -> http::request::Builder {
        hyper::Request::builder()
            .uri(uri.to_string())
            .header("X-Client-Name", self.client_name.to_string())
            .header("X-Client-Version", self.client_version.to_string())
    }

    /// Returns a request builder for a GET request.
    fn get(&self, uri: Url) -> http::request::Builder {
        self.request_builder(uri).method(hyper::Method::GET)
    }

    /// Performs a request.
    fn raw_request(&self, req: hyper::Request<hyper::Body>) -> ResponseFuture {
        self.inner.request(req)
    }
}

impl HorizonClient for HorizonHttpClient {
    fn request<'a, R: Request + 'a>(&'a self, req: R) -> BoxFuture<'a, Result<R::Response>> {
        Box::pin(execute_request(self, req))
    }

    fn stream<'a, 'b, R: StreamRequest + 'a>(
        &'a self,
        request: R,
    ) -> Result<Box<dyn Stream<Item = Result<R::Resource>> + 'a + Unpin>> {
        Ok(Box::new(HorizonHttpStream {
            client: &self,
            request,
            last_id: None,
            response: None,
            decoder: None,
        }))
    }
}

async fn execute_request<R: Request>(client: &HorizonHttpClient, req: R) -> Result<R::Response> {
    let http_method = if req.is_post() {
        hyper::Method::POST
    } else {
        hyper::Method::GET
    };
    let uri = req.uri(&client.host)?;
    let request = client
        .request_builder(uri)
        .method(http_method)
        .body(hyper::Body::empty())?;

    let response = client.raw_request(request).await?;

    if response.status().is_success() {
        let bytes = hyper::body::to_bytes(response).await?;
        let result: R::Response = serde_json::from_slice(&bytes)?;
        Ok(result)
    } else if response.status().is_client_error() {
        let bytes = hyper::body::to_bytes(response).await?;
        let result: HorizonError = serde_json::from_slice(&bytes)?;
        Err(Error::HorizonRequestError(result))
    } else {
        Err(Error::HorizonServerError)
    }
}

impl<'a, R> Stream for HorizonHttpStream<'a, R>
where
    R: StreamRequest,
{
    type Item = Result<R::Resource>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        loop {
            if self.response.is_none() && self.decoder.is_none() {
                let uri = self.request.uri(&self.client.host)?;
                let mut request_builder =
                    self.client.get(uri).header("Accept", "text/event-stream");
                if let Some(last_id) = &self.last_id {
                    request_builder = request_builder.header("Last-Event-Id", last_id.clone());
                }

                let request = request_builder.body(hyper::Body::empty())?;
                let response = self.client.raw_request(request);
                self.response = Some(response);
            }

            if let Some(mut resp) = self.response.take() {
                match Pin::new(&mut resp).poll(cx) {
                    Poll::Pending => {
                        self.response = Some(resp);
                        return Poll::Pending;
                    }
                    Poll::Ready(Err(e)) => {
                        return Poll::Ready(Some(Err(e.into())));
                    }
                    Poll::Ready(Ok(resp)) => {
                        // TODO(fra): handle non success statuses
                        assert!(resp.status().is_success());
                        let body_stream = resp
                            .into_body()
                            .map_err(|e| futures::io::Error::new(futures::io::ErrorKind::Other, e))
                            .into_async_read();

                        let decoder = Box::new(async_sse::decode(body_stream));
                        self.decoder = Some(decoder);
                    }
                }
            }

            if let Some(mut decoder) = self.decoder.take() {
                match Pin::new(&mut decoder).poll_next(cx) {
                    Poll::Pending => {
                        self.decoder = Some(decoder);
                        return Poll::Pending;
                    }
                    Poll::Ready(None) => {}
                    Poll::Ready(Some(Err(_))) => {
                        let err = Error::SSEDecoderError;
                        return Poll::Ready(Some(Err(err)));
                    }
                    Poll::Ready(Some(Ok(message))) => {
                        self.decoder = Some(decoder);
                        match message {
                            async_sse::Event::Message(msg) => {
                                if let Some(last_id) = msg.id() {
                                    self.last_id = Some(last_id.to_string());
                                }
                                if msg.name() == "message" {
                                    let result: R::Resource =
                                        serde_json::from_slice(&msg.into_bytes())?;
                                    return Poll::Ready(Some(Ok(result)));
                                }
                            }
                            async_sse::Event::Retry(duration) => {
                                println!("got duration {:?}", duration);
                            }
                        }
                    }
                }
            }
        }
    }
}