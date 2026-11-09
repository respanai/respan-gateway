//! A provider of a vendor, ready to register: each format at its base URL,
//! taking the key as the format takes it; and its model list.

use std::sync::Arc;

use crate::{Connect, KeyIn, Models, Reach, Upstream, Vendor};
use respan_gateway_core::{Fault, Gateway};
use respan_gateway_http::http::HeaderMap;
use respan_gateway_http::http::header::{AUTHORIZATION, HeaderName, HeaderValue};
use respan_gateway_http_transport::{HttpTransport, Network, reqwest, unreachable};
use respan_gateway_ws::{Limits, WsTransport};

/// A provider of a vendor ([`Vendor::provider`]), or of formats of no
/// vendor, each at a base URL, with a key, to register
/// ([`register`](VendorProvider::register)).
///
/// ```ignore
/// let openai = respan_gateway_vendors::openai::VENDOR.provider("openai");
/// let gateway = openai.register(Gateway::new())?;
/// let models = openai.models().await;
/// let usage = openai.client()?.send(request).await?; // any call of its own
/// ```
#[derive(Clone)]
pub struct VendorProvider {
    name: String,
    vendor: &'static Vendor,
    /// Each format it speaks, at its base URL, if it has one yet.
    formats: Vec<(&'static dyn Upstream, Option<At>)>,
    key: Key,
    limit: usize,
    sessions: Limits,
    network: Network,
    /// The client of its calls over HTTP, in place of its network's.
    client: Option<reqwest::Client>,
}

/// One format's core provider of a [`VendorProvider`] ([`VendorProvider::parts`]).
#[derive(Debug)]
pub struct Part {
    /// Its name: the provider's for its first format, `name:format` for the
    /// others.
    pub name: String,
    /// Its format.
    pub format: &'static dyn Upstream,
    /// Its base URL, in its scheme: the one it was given; or, of one its
    /// provider's formats share, the origin if its paths are whole
    /// ([`Upstream::at_origin`]).
    pub base_url: String,
    /// What its calls carry to authenticate.
    pub headers: HeaderMap,
}

/// Where a format is.
#[derive(Clone, Debug)]
enum At {
    /// At a base URL of its own, as it is.
    Own(String),
    /// At the base URL its provider's formats share, its API's: a format
    /// of whole paths takes its origin.
    Shared(String),
}

impl At {
    fn url(&self) -> &str {
        match self {
            At::Own(url) | At::Shared(url) => url,
        }
    }
}

/// What a provider's calls authenticate with.
#[derive(Clone)]
enum Key {
    /// The client's own credentials.
    Client,
    /// A key, sent as each format takes it.
    Own(String),
    /// These headers, as they are.
    Headers(HeaderMap),
}

impl std::fmt::Debug for VendorProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VendorProvider")
            .field("name", &self.name)
            .field("vendor", &self.vendor.id)
            .finish_non_exhaustive()
    }
}

impl Vendor {
    /// Provider `name` of this vendor: every format it speaks, each at its
    /// base URL (none for one whose URL names an environment variable not
    /// set), with its key from the first of `key_env` set, else the
    /// client's.
    pub fn provider(&'static self, name: impl Into<String>) -> VendorProvider {
        let env = |var: &&str| std::env::var(var).ok().filter(|k| !k.trim().is_empty());
        let key = self.key_env.iter().find_map(env);
        let at = |url: Option<&str>| url.and_then(|url| (self.locate)(url).ok()).map(At::Own);
        VendorProvider {
            vendor: self,
            formats: self.formats.iter().map(|(f, url)| (*f, at(*url))).collect(),
            key: key.map_or(Key::Client, Key::Own),
            ..VendorProvider::new(name)
        }
    }
}

impl VendorProvider {
    /// Provider `name` of no vendor and no formats yet
    /// ([`formats`](Self::formats)), with the client's credentials.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            vendor: &NONE,
            formats: Vec::new(),
            key: Key::Client,
            limit: respan_gateway_http::call::MAX_BODY,
            sessions: Limits::default(),
            network: Network::shared().clone(),
            client: None,
        }
    }

    /// Speaking `formats` alone, each at its own base URL if it has one, in
    /// place of its vendor's.
    pub fn formats(
        mut self,
        formats: impl IntoIterator<Item = (&'static dyn Upstream, Option<String>)>,
    ) -> Self {
        let formats = formats.into_iter();
        self.formats = formats.map(|(f, url)| (f, url.map(At::Own))).collect();
        self
    }

    /// Every format at `base_url`: its API's base, which a format of whole
    /// paths takes at its origin; of a provider of one format, that
    /// format's own.
    pub fn base(mut self, base_url: impl Into<String>) -> Self {
        let base_url = base_url.into();
        let shared = self.formats.len() > 1;
        for (_, at) in &mut self.formats {
            *at = Some(match shared {
                true => At::Shared(base_url.clone()),
                false => At::Own(base_url.clone()),
            });
        }
        self
    }

    /// Its format `format` at `base_url`, its own; `Err` with `config` if it
    /// speaks no format of that name.
    pub fn base_of(mut self, format: &str, base_url: impl Into<String>) -> Result<Self, Fault> {
        let found = self.formats.iter_mut().find(|(f, _)| f.name() == format);
        let (_, at) = found.ok_or_else(|| Fault::config(format!("speaks no format {format}")))?;
        *at = Some(At::Own(base_url.into()));
        Ok(self)
    }

    /// The base URL its format `format` was given, if it speaks it and has
    /// one.
    pub fn base_url_of(&self, format: &str) -> Option<&str> {
        let found = self.formats.iter().find(|(f, _)| f.name() == format);
        found.and_then(|(_, at)| at.as_ref()).map(At::url)
    }

    /// With `key`, sent as each format takes it.
    pub fn key(mut self, key: impl Into<String>) -> Self {
        self.key = Key::Own(key.into());
        self
    }

    /// With `headers` on every call, as they are, in place of a key.
    pub fn headers(mut self, headers: HeaderMap) -> Self {
        self.key = Key::Headers(headers);
        self
    }

    /// Reading up to `bytes` of one answer at once.
    pub fn limit(mut self, bytes: usize) -> Self {
        self.limit = bytes;
        self
    }

    /// Its sessions taking `limits`.
    pub fn sessions(mut self, limits: Limits) -> Self {
        self.sessions = limits;
        self
    }

    /// Connecting through `network`: its proxy, and the certificates it
    /// trusts.
    pub fn network(mut self, network: Network) -> Self {
        self.network = network;
        self
    }

    /// Making its calls over HTTP with `client`, one of your own, in place
    /// of its network's; its sessions over WebSocket keep the network's.
    /// Have it follow no redirects
    /// ([`reqwest::redirect::Policy::none`]).
    pub fn http_client(mut self, client: reqwest::Client) -> Self {
        self.client = Some(client);
        self
    }

    /// Its vendor: [`Vendor::DEFAULT`]'s, of no id, for one of no vendor.
    pub fn vendor(&self) -> &'static Vendor {
        self.vendor
    }

    /// Each format's core provider. `Err` with `config` for a format with
    /// no base URL, or a key no header can carry.
    pub fn parts(&self) -> Result<Vec<Part>, Fault> {
        let part = |(i, (format, at)): (usize, &(&'static dyn Upstream, Option<At>))| {
            let at = at.as_ref().ok_or_else(|| {
                Fault::config(format!("format {} needs a `base_url`", format.name()))
            })?;
            Ok(Part {
                name: match i {
                    0 => self.name.clone(),
                    _ => format!("{}:{}", self.name, format.name()),
                },
                format: *format,
                base_url: base_url(*format, at),
                headers: self.auth(format.key())?,
            })
        };
        self.formats.iter().enumerate().map(part).collect()
    }

    /// `gateway` with a core provider of each of its formats.
    pub fn register(&self, mut gateway: Gateway) -> Result<Gateway, Fault> {
        for part in self.parts()? {
            let format = part.format;
            let connect = self.connect(format, &part.base_url, part.headers)?;
            gateway = gateway.provider(format.provider(part.name, connect))?;
        }
        Ok(gateway)
    }

    /// A client of it: each call to the base URL of its first format of an
    /// API over HTTP, with its key as that format takes it.
    pub fn client(&self) -> Result<HttpTransport, Fault> {
        let part = self.api()?;
        self.keyless_client_at(&part.base_url)?
            .with_headers(part.headers)
    }

    /// A client of it at `base_url`, such as an API beside the one its
    /// formats are at, with its key as its first format of an API over HTTP
    /// takes it.
    pub fn client_at(&self, base_url: &str) -> Result<HttpTransport, Fault> {
        self.keyless_client_at(base_url)?
            .with_headers(self.api()?.headers)
    }

    /// A client of it at the base URL of its format `format`, if it speaks
    /// it.
    pub fn client_of(&self, format: &str) -> Result<Option<HttpTransport>, Fault> {
        let part = self
            .parts()?
            .into_iter()
            .find(|p| p.format.name() == format);
        let client = |p: Part| self.keyless_client_at(&p.base_url)?.with_headers(p.headers);
        part.map(client).transpose()
    }

    /// The models it lists, as its vendor lists them ([`Vendor::models`]);
    /// `None` if it speaks no format of an API over HTTP, such as only
    /// passthrough or sessions.
    pub async fn models(&self) -> Option<Models> {
        if !self.formats.iter().any(|(f, _)| of_api(*f)) {
            return None;
        }
        let listed = (self.vendor.models)(self).await;
        Some(listed.map_err(|f| unreached(&f).map_or(f, Fault::upstream)))
    }

    /// A client of it at `base_url` carrying no key, such as for a page
    /// that needs none, through its network or with its client.
    pub fn keyless_client_at(&self, base_url: &str) -> Result<HttpTransport, Fault> {
        let transport = HttpTransport::new(base_url)?;
        match &self.client {
            Some(client) => Ok(transport.with_client(client.clone())),
            None => transport.with_network(&self.network),
        }
    }

    /// Its first format of an API over HTTP.
    fn api(&self) -> Result<Part, Fault> {
        let part = self.parts()?.into_iter().find(|p| of_api(p.format));
        part.ok_or_else(|| Fault::config("speaks no format of an API over HTTP"))
    }

    /// The headers its calls of a format that takes the key at `key` carry.
    fn auth(&self, key: KeyIn) -> Result<HeaderMap, Fault> {
        let (name, value) = match (&self.key, key) {
            (Key::Client, _) => return Ok(HeaderMap::new()),
            (Key::Headers(headers), _) => return Ok(headers.clone()),
            (Key::Own(key), KeyIn::Bearer) => (AUTHORIZATION, format!("Bearer {key}")),
            (Key::Own(key), KeyIn::Header(name)) => (HeaderName::from_static(name), key.clone()),
        };
        let mut value = HeaderValue::from_str(&value).map_err(|e| Fault::config(e.to_string()))?;
        value.set_sensitive(true);
        Ok(HeaderMap::from_iter([(name, value)]))
    }

    /// What makes the calls of `format` at `base`, carrying `auth`: a
    /// session over WebSocket; or calls over HTTP, a provider it cannot
    /// reach named in the fault with its host, as the transport's names
    /// neither.
    fn connect(
        &self,
        format: &dyn Upstream,
        base: &str,
        auth: HeaderMap,
    ) -> Result<Connect, Fault> {
        let follow = match format.reach() {
            Reach::Session { binary_text } => {
                let transport = WsTransport::new(base)?
                    .with_network(&self.network)?
                    .with_binary_text(binary_text)
                    .with_limits(self.sessions);
                return Ok(Arc::new(transport.with_headers(auth)?.connect()));
            }
            Reach::Http { follow } => follow,
        };
        let mut client = self.keyless_client_at(base)?;
        if let Some(follow) = follow {
            client = client.follow_redirects(follow);
        }
        let client = Arc::new(client.with_headers(auth)?);
        let host = base.split_once("://").map_or(base, |(_, rest)| rest);
        let host = host.split('/').next().unwrap_or_default();
        let who: Arc<str> = format!("provider '{}' at {host}", self.name).into();
        Ok(Arc::new(respan_gateway_http::connect(
            move |call| {
                let (client, who) = (client.clone(), who.clone());
                async move {
                    client
                        .send(call)
                        .await
                        .map_err(|fault| match unreached(&fault) {
                            Some(why) => Fault::upstream(format!("{who} {why}")).with_source(fault),
                            None => fault,
                        })
                }
            },
            self.limit,
        )))
    }
}

/// The vendor of a provider of no vendor.
static NONE: Vendor = Vendor::DEFAULT;

/// Whether `format` is of the provider's API over HTTP: not a session's,
/// nor one of whole paths, such as the passthrough's.
fn of_api(format: &dyn Upstream) -> bool {
    !matches!(format.reach(), Reach::Session { .. }) && !format.at_origin()
}

/// `format`'s base URL, at `at`: its own as it is, a shared one at its
/// origin if its paths are whole; in its scheme, a session's `ws`/`wss`,
/// another's `http`/`https`.
fn base_url(format: &dyn Upstream, at: &At) -> String {
    let base = at.url().trim_end_matches('/');
    let base = match (at, format.at_origin()) {
        (At::Shared(_), true) => origin(base),
        _ => base,
    };
    let swap = match format.reach() {
        Reach::Session { .. } => [("https://", "wss://"), ("http://", "ws://")],
        Reach::Http { .. } => [("wss://", "https://"), ("ws://", "http://")],
    };
    swap.iter()
        .find_map(|(from, to)| base.strip_prefix(from).map(|rest| format!("{to}{rest}")))
        .unwrap_or_else(|| base.to_owned())
}

/// `scheme://host[:port]` of `url`.
fn origin(url: &str) -> &str {
    let after_scheme = url.find("://").map_or(0, |i| i + 3);
    match url[after_scheme..].find('/') {
        Some(slash) => &url[..after_scheme + slash],
        None => url,
    }
}

/// Why `fault` says its provider could not be reached, if it does.
pub(crate) fn unreached(fault: &Fault) -> Option<String> {
    let e = fault.cause::<reqwest::Error>();
    e.filter(|e| !e.is_builder()).map(unreachable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::located;

    /// A format of no kind, over HTTP or as a session, its paths whole if
    /// `whole`.
    struct Fake {
        session: bool,
        whole: bool,
    }

    impl Upstream for Fake {
        fn name(&self) -> &str {
            "fake"
        }
        fn reach(&self) -> Reach {
            match self.session {
                true => Reach::Session { binary_text: false },
                false => Reach::Http { follow: None },
            }
        }
        fn at_origin(&self) -> bool {
            self.whole
        }
        fn provider(&self, _: String, _: Connect) -> respan_gateway_core::Provider {
            unreachable!("not registered")
        }
    }

    /// Where `format` sits, at `base` of its own, or shared with another.
    fn at(format: &'static dyn Upstream, base: &str, shared: bool) -> String {
        let other: &'static dyn Upstream = &Fake {
            session: false,
            whole: false,
        };
        let formats = match shared {
            true => vec![(format, None), (other, None)],
            false => vec![(format, None)],
        };
        let p = VendorProvider::new("p").formats(formats).base(base);
        p.parts().map(|p| p[0].base_url.clone()).unwrap_or_default()
    }

    #[test]
    fn a_format_sits_at_its_base_url_in_its_scheme() {
        let http = &Fake {
            session: false,
            whole: false,
        };
        let session = &Fake {
            session: true,
            whole: false,
        };
        assert_eq!(at(http, "https://x.test/v1/", false), "https://x.test/v1");
        assert_eq!(at(session, "https://x.test/v1", true), "wss://x.test/v1");
        let none = VendorProvider::new("p").formats([(http as &dyn Upstream, None)]);
        let fault = none.parts().err().map(|f| f.message().to_owned());
        assert_eq!(fault.as_deref(), Some("format fake needs a `base_url`"));
    }

    /// A format of whole paths takes the origin of a base URL its
    /// provider's formats share, its API's, but one of its own as it is.
    #[test]
    fn a_format_of_whole_paths_takes_a_shared_base_urls_origin() {
        let whole = &Fake {
            session: false,
            whole: true,
        };
        assert_eq!(at(whole, "https://x.test/v1", true), "https://x.test");
        assert_eq!(at(whole, "https://x.test/pt", false), "https://x.test/pt");
    }

    #[test]
    fn a_base_url_takes_its_names_from_the_environment() {
        let home = std::env::var("HOME").unwrap_or_default();
        assert_eq!(
            located("https://x.test/{HOME}/v1"),
            Ok(format!("https://x.test/{home}/v1"))
        );
        assert_eq!(
            located("https://x.test/{RESPAN_NOT_SET}"),
            Err("RESPAN_NOT_SET")
        );
    }
}
