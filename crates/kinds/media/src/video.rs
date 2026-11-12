//! The video kind: video generation as a job, created, then polled, then
//! downloaded.
//!
//! Each request is one operation on a job ([`Input`]); a response is a
//! sequence of events: a [`Output::Job`] for one job's state, or the
//! content's bytes in [`Output::Content`] chunks, with [`Output::Native`]
//! among them.
//!
//! A job's id is the provider's, opaque, as a client gives it back: a
//! follow-up (`Retrieve`, `Content`, or a format's own operation on a job)
//! has to reach the provider that made the job, which the id alone says
//! only by its spelling ([`Input::job_id`]). Some providers serve a finished
//! job's content only at the URLs it names ([`Job::videos`]);
//! [`LocateContent`] finds them first.
//! [`VideoJobs`] sends a follow-up to its job's provider.

use bytes::Bytes;
use respan_gateway_conversation::{Media, MediaSource};
use respan_gateway_core::futures::StreamExt;
use respan_gateway_core::{
    ClientContext, ErrorCode, Fault, FormatId, NativeFields, NativeValue, Plugin, Response,
    async_trait,
};
use serde_json::Value;

/// The kind's input: one operation on a video job.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
// A request is one operation: its size costs nothing.
#[allow(clippy::large_enum_variant)]
pub enum Input {
    /// Start a job making a video.
    Create(Create),
    /// A job's state.
    Retrieve {
        /// The job.
        id: String,
    },
    /// A finished job's video.
    Content {
        /// The job.
        id: String,
        /// Where the provider serves it, if the job named it
        /// ([`Job::videos`]); a provider that serves content only there
        /// needs it.
        url: Option<String>,
    },
    /// An operation the kind has no form for, as its format has it, such
    /// as OpenAI's remix, delete, list or thumbnail: only a provider of that
    /// format can make it.
    Native {
        /// The job it is on, or a page starts after, if any.
        id: Option<String>,
        /// The operation.
        native: NativeValue,
    },
}

impl Input {
    /// The model a `Create` asks for; no other operation names one.
    pub fn model(&self) -> Option<&str> {
        match self {
            Input::Create(create) => create.model.as_deref(),
            _ => None,
        }
    }

    /// Asks for `model`, if a `Create`; no other operation names one, so
    /// the rest are left as they are.
    pub fn set_model(&mut self, model: String) {
        if let Input::Create(create) = self {
            create.model = Some(model);
        }
    }

    /// The job an operation follows: the one it is on, or the one a page
    /// starts after; `None` for `Create`.
    pub fn job_id(&self) -> Option<&str> {
        match self {
            Input::Retrieve { id } | Input::Content { id, .. } => Some(id),
            Input::Native { id, .. } => id.as_deref(),
            Input::Create(_) => None,
        }
    }
}

/// A request for a video.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct Create {
    /// The model asked for, if any; without one, the provider's default.
    pub model: Option<String>,
    /// What to show.
    pub prompt: String,
    /// How long, in seconds.
    pub seconds: Option<u32>,
    /// Its size, width by height, such as `1280x720`.
    pub size: Option<String>,
    /// An image to start from.
    pub reference: Option<Media>,
    /// The request's fields the kind has no place for.
    pub native: Option<NativeFields>,
}

impl Create {
    /// A video of `prompt`.
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            ..Self::default()
        }
    }
}

/// The kind's output: one event of a response.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
// A response holds one job at most: its size costs nothing.
#[allow(clippy::large_enum_variant)]
pub enum Output {
    /// A job's state.
    Job(Job),
    /// The next bytes of the content.
    Content(Bytes),
    /// An answer the kind has no form for, as its format has it, such as
    /// OpenAI's page of jobs or deletion: only a client of that format gets
    /// it.
    Native {
        /// The jobs' ids it names, in its format's order; the format writes
        /// them back in that order.
        ids: Vec<String>,
        /// The answer.
        native: NativeValue,
    },
}

impl Output {
    /// An event of `format` the kind has no form for, naming no job.
    pub fn native(format: impl Into<FormatId>, raw: impl Into<Value>) -> Self {
        Output::Native {
            ids: Vec::new(),
            native: NativeValue::new(format, raw),
        }
    }
}

/// A video job's state.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Job {
    /// Its id, as the provider's format gives it.
    pub id: String,
    /// Where it is.
    pub status: Status,
    /// The model making it.
    // WARN: openrouter-videos names none.
    pub model: Option<String>,
    /// When its content will be gone, in seconds since the Unix epoch.
    // WARN: gemini-veo says none.
    pub expires_at: Option<u64>,
    /// The video's length, in seconds.
    // WARN: openrouter-videos says none.
    pub seconds: Option<u32>,
    /// The video's size, width by height.
    // WARN: openrouter-videos says none.
    pub size: Option<String>,
    /// Why it failed.
    pub error: Option<JobError>,
    /// Where the provider serves the videos it made, if it names them.
    // WARN: openai-videos names none: it serves them at `/content`.
    pub videos: Vec<Media>,
    /// Its fields the kind has no place for.
    pub native: Option<NativeFields>,
}

impl Job {
    /// The job `id`, at `status`.
    pub fn new(id: impl Into<String>, status: Status) -> Self {
        Self {
            id: id.into(),
            status,
            model: None,
            expires_at: None,
            seconds: None,
            size: None,
            error: None,
            videos: Vec::new(),
            native: None,
        }
    }

    /// The URL of its first video, if the provider named one.
    pub fn video_url(&self) -> Option<&str> {
        self.videos.iter().find_map(|media| match &media.source {
            MediaSource::Url(url) => Some(url.as_str()),
            MediaSource::Bytes(_) => None,
        })
    }
}

/// Where a job is.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Status {
    /// Waiting to start.
    Queued,
    /// Being made.
    InProgress,
    /// Made: its content is ready.
    Completed,
    /// Given up ([`Job::error`]).
    Failed,
    /// A state the kind has no form for, as `format` names it; only a
    /// response in that format can say it.
    #[non_exhaustive]
    Native {
        /// The format that names it.
        format: FormatId,
        /// The state, as that format names it.
        status: String,
    },
}

impl Status {
    /// The state `status` of `format`, which the kind has no form for.
    pub fn native(format: impl Into<FormatId>, status: impl Into<String>) -> Self {
        Status::Native {
            format: format.into(),
            status: status.into(),
        }
    }
}

/// Why a job failed.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct JobError {
    /// What failed, as the provider names it.
    // WARN: openrouter-videos names none.
    pub code: Option<String>,
    /// What the provider says of it.
    pub message: String,
}

impl JobError {
    /// A failure the provider says `message` of.
    pub fn new(code: Option<String>, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// For a provider that serves a finished job's content only at the URLs
/// the job names ([`Job::videos`]), as Veo does: a `Content` request without
/// a `url` whose target is one of its providers first retrieves the job
/// from it, then goes on with the URL of the job's first video. A job not
/// finished is refused, 400, as content not ready; register it after the
/// router that picks the target.
#[derive(Debug, Clone, Default)]
pub struct LocateContent {
    providers: Vec<String>,
}

impl LocateContent {
    /// For no provider yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// For `provider` too.
    pub fn provider(mut self, provider: impl Into<String>) -> Self {
        self.providers.push(provider.into());
        self
    }
}

#[async_trait]
impl Plugin for LocateContent {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>, Fault> {
        let Ok(Input::Content { id, url: None }) = cx.input::<Input>() else {
            return Ok(None);
        };
        let Some(target) = cx
            .target()
            .filter(|t| self.providers.iter().any(|p| p == t))
        else {
            return Ok(None);
        };
        let (id, target) = (id.clone(), target.to_owned());
        let retrieved = cx.call(target).with_input(Input::Retrieve { id }).await?;
        let mut outputs = retrieved.outputs::<Output>();
        let mut job = None;
        while let Some(output) = outputs.next().await {
            if let Output::Job(found) = output? {
                job = Some(found);
            }
        }
        let job = job.ok_or_else(|| Fault::upstream("the provider retrieved no job"))?;
        let url = match (&job.status, job.video_url()) {
            (Status::Completed, Some(url)) => url.to_owned(),
            (Status::Completed, None) => {
                return Err(Fault::upstream(
                    "the provider's finished job names no video",
                ));
            }
            (status, _) => {
                return Err(Fault::bad_request(format!(
                    "the video is not ready: its job is {status:?}"
                ))
                .with(ErrorCode("video_not_ready".into())));
            }
        };
        cx.filter(move |input: &mut Input| {
            if let Input::Content { url: at, .. } = input {
                *at = Some(url.clone());
            }
            Ok(())
        })?;
        Ok(None)
    }
}

/// What wraps a job's id in its provider's name: `<provider>~<id>`.
const SEPARATOR: char = '~';

/// A core provider of a video format, of [`VideoJobs`].
#[derive(Debug, Clone)]
struct Maker {
    provider: String,
    internal: String,
    /// What its jobs' ids start with.
    spelling: String,
    /// Whether another provider's ids are spelled alike.
    shared: bool,
}

/// Sends a job's follow-up (`Retrieve`, `Content`, and a native operation
/// on it or a page after it) to the provider that made the job, which only
/// the job's id says: by its spelling (OpenAI's start `video_`, Veo's `veo_`), or, when several
/// providers spell their ids alike, by the provider's name it wraps them in
/// on the way out, `<provider>~<id>`, and unwraps on the way in. An id
/// spelled as several providers' goes to the target if it is one of them,
/// else to the first. Register it after the router that picks the target.
#[derive(Debug, Clone, Default)]
pub struct VideoJobs {
    makers: Vec<Maker>,
}

impl VideoJobs {
    /// For no provider yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// For `internal` too, a core provider of the provider named `provider`
    /// (the name a wrapped id carries), whose jobs' ids start with
    /// `spelling`.
    pub fn provider(
        mut self,
        provider: impl Into<String>,
        internal: impl Into<String>,
        spelling: impl Into<String>,
    ) -> Self {
        self.makers.push(Maker {
            provider: provider.into(),
            internal: internal.into(),
            spelling: spelling.into(),
            shared: false,
        });
        let spellings: Vec<String> = self.makers.iter().map(|m| m.spelling.clone()).collect();
        for maker in &mut self.makers {
            maker.shared = spellings.iter().filter(|s| **s == maker.spelling).count() > 1;
        }
        self
    }

    /// Who made job `id`, and its id as they know it: the provider it is
    /// wrapped in, else the target if it spells ids so, else the first that
    /// does.
    fn maker<'a>(&self, id: &'a str, target: Option<&str>) -> Option<(&Maker, &'a str)> {
        if let Some((provider, inner)) = id.split_once(SEPARATOR)
            && let Some(maker) = self.makers.iter().find(|m| m.provider == provider)
        {
            return Some((maker, inner));
        }
        let mut spelled = self
            .makers
            .iter()
            .filter(|m| id.starts_with(m.spelling.as_str()));
        let targeted = spelled
            .clone()
            .find(|m| target == Some(m.internal.as_str()));
        targeted.or_else(|| spelled.next()).map(|m| (m, id))
    }
}

#[async_trait]
impl Plugin for VideoJobs {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>, Fault> {
        let Ok(input) = cx.input::<Input>() else {
            return Ok(None);
        };
        let found = input.job_id().and_then(|id| {
            let (maker, inner) = self.maker(id, cx.target())?;
            Some((
                maker.internal.clone(),
                (inner != id).then(|| inner.to_owned()),
            ))
        });
        if let Some((internal, unwrapped)) = found {
            cx.set_target(internal);
            if let Some(inner) = unwrapped {
                cx.filter(move |input: &mut Input| {
                    set_job_id(input, &inner);
                    Ok(())
                })?;
            }
        }
        let target = cx.target();
        let maker = self.makers.iter().find(|m| target == Some(&m.internal));
        if let Some(maker) = maker.filter(|m| m.shared) {
            let provider = maker.provider.clone();
            cx.filter(move |output: &mut Output| {
                wrap_ids(output, &provider);
                Ok(())
            })?;
        }
        Ok(None)
    }
}

fn set_job_id(input: &mut Input, inner: &str) {
    match input {
        Input::Retrieve { id, .. }
        | Input::Content { id, .. }
        | Input::Native { id: Some(id), .. } => *id = inner.to_owned(),
        _ => {}
    }
}

/// The ids of `output` wrapped in `provider`: a job's, and a native answer's.
// WARN: a remix's source (openai-videos' own `remixed_from_video_id`) is not
// wrapped: the kind has no place for it, and its format's fields are its own.
fn wrap_ids(output: &mut Output, provider: &str) {
    let wrap = |id: &mut String| *id = format!("{provider}{SEPARATOR}{id}");
    match output {
        Output::Job(job) => wrap(&mut job.id),
        Output::Native { ids, .. } => ids.iter_mut().for_each(wrap),
        Output::Content(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jobs() -> VideoJobs {
        VideoJobs::new()
            .provider("east", "east:openai-videos", "video_")
            .provider("west", "west:openai-videos", "video_")
            .provider("gemini", "gemini:gemini-veo", "veo_")
    }

    fn maker(id: &str, target: Option<&str>) -> Option<(String, String)> {
        let jobs = jobs();
        let (maker, inner) = jobs.maker(id, target)?;
        Some((maker.provider.clone(), inner.to_owned()))
    }

    #[test]
    fn a_job_goes_to_its_maker() {
        let to = |provider: &str, id: &str| Some((provider.to_owned(), id.to_owned()));
        assert_eq!(maker("west~video_1", None), to("west", "video_1"));
        assert_eq!(maker("veo_abc", Some("east")), to("gemini", "veo_abc"));
        assert_eq!(maker("video_1", None), to("east", "video_1"));
        assert_eq!(
            maker("video_1", Some("west:openai-videos")),
            to("west", "video_1")
        );
        assert_eq!(maker("nobody~video_1", None), None);
        assert_eq!(maker("file_1", None), None);
    }

    #[test]
    fn only_ids_spelled_alike_are_wrapped() {
        let shared: Vec<_> = jobs().makers.iter().map(|m| m.shared).collect();
        assert_eq!(shared, [true, true, false]);
    }

    #[test]
    fn ids_are_wrapped_in_their_provider() {
        let page = |ids: &[&str]| Output::Native {
            ids: ids.iter().map(|id| id.to_string()).collect(),
            native: NativeValue::new("openai-videos", Value::Null),
        };
        let mut listed = page(&["video_1", "video_2"]);
        wrap_ids(&mut listed, "east");
        assert_eq!(listed, page(&["east~video_1", "east~video_2"]));
        let mut input = Input::Retrieve {
            id: "east~video_1".into(),
        };
        set_job_id(&mut input, "video_1");
        assert_eq!(input.job_id(), Some("video_1"));
    }

    /// Regression: a page's `after` kept the wrapping its id was given out
    /// in, and went to whichever provider the router picked.
    #[test]
    fn a_page_after_a_wrapped_job_goes_to_its_maker_unwrapped() {
        let list = |after: Option<&str>| Input::Native {
            id: after.map(str::to_owned),
            native: NativeValue::new("openai-videos", Value::Null),
        };
        let mut input = list(Some("west~video_1"));
        let found = input
            .job_id()
            .and_then(|id| maker(id, Some("east:openai-videos")));
        assert_eq!(found, Some(("west".to_owned(), "video_1".to_owned())));
        set_job_id(&mut input, "video_1");
        assert_eq!(input.job_id(), Some("video_1"));
        assert_eq!(list(None).job_id(), None);
    }
}
