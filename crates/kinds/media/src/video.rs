//! The video kind: video generation as a job, created, then polled, then
//! downloaded.
//!
//! Each request is one operation on a job ([`Input`]); a response is a
//! sequence of events: a [`Output::Job`] for one job's state, a
//! [`Output::Jobs`] for a page of them, the content's bytes in
//! [`Output::Content`] chunks, or [`Output::Deleted`], with
//! [`Output::Native`] among them.
//!
//! A job's id is the provider's, opaque, as a client gives it back: a
//! follow-up (`Retrieve`, `Content`, `Remix`, `Delete`) has to reach the
//! provider that made the job, which the id alone says only by its spelling
//! ([`Input::job_id`]). Some providers serve a finished job's content only at
//! the URLs it names ([`Job::videos`]); [`LocateContent`] finds them first.
//! [`VideoJobs`] sends a follow-up to its job's provider.

use bytes::Bytes;
use respan_gateway_conversation::{Media, MediaSource};
use respan_gateway_core::futures::StreamExt;
use respan_gateway_core::{
    ClientContext, ErrorCode, Fault, FormatId, NativeFields, NativeValue, Plugin, ProviderContext,
    Response, async_trait,
};
use serde_json::Value;

/// The kind's input: one operation on a video job.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Input {
    /// Start a job making a video.
    Create(Create),
    /// Start a job remaking the finished video `id` as `prompt` says.
    Remix {
        /// The video to remake.
        id: String,
        /// What to change.
        prompt: String,
        /// The request's fields the kind has no place for.
        native: Option<NativeFields>,
    },
    /// A job's state.
    Retrieve {
        /// The job.
        id: String,
    },
    /// A finished job's content.
    Content {
        /// The job.
        id: String,
        /// Which of its content.
        variant: Variant,
        /// Where the provider serves it, if the job named it
        /// ([`Job::videos`]); a provider that serves content only there
        /// needs it.
        url: Option<String>,
    },
    /// Delete a job and its content.
    Delete {
        /// The job.
        id: String,
    },
    /// A page of jobs, newest first unless `order` says otherwise.
    List {
        /// The id of the job the page starts after.
        after: Option<String>,
        /// How many jobs, at most.
        limit: Option<u32>,
        /// `asc` or `desc`, by when they were made.
        order: Option<String>,
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

    /// The job an operation is on: `None` for `Create` and `List`.
    pub fn job_id(&self) -> Option<&str> {
        match self {
            Input::Remix { id, .. }
            | Input::Retrieve { id }
            | Input::Content { id, .. }
            | Input::Delete { id } => Some(id),
            Input::Create(_) | Input::List { .. } => None,
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

/// Which of a finished job's content.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Variant {
    /// The video.
    #[default]
    Video,
    /// A still of it.
    Thumbnail,
    /// Stills of it in one image.
    Spritesheet,
}

/// The kind's output: one event of a response.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
// A response holds one job at most: its size costs nothing.
#[allow(clippy::large_enum_variant)]
pub enum Output {
    /// A job's state.
    Job(Job),
    /// A page of jobs.
    Jobs(Jobs),
    /// The next bytes of the content.
    Content(Bytes),
    /// The job deleted.
    Deleted(String),
    /// An event the kind has no form for, as `format` has it. Sent only in
    /// that format.
    Native(NativeValue),
}

impl Output {
    /// An event of `format` the kind has no form for.
    pub fn native(format: impl Into<FormatId>, raw: impl Into<Value>) -> Self {
        Output::Native(NativeValue::new(format, raw))
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
    pub model: Option<String>,
    /// How far along, in percent.
    pub progress: Option<u32>,
    /// When it was made, in seconds since the Unix epoch.
    pub created_at: Option<u64>,
    /// When it finished.
    pub completed_at: Option<u64>,
    /// When its content will be gone.
    pub expires_at: Option<u64>,
    /// The video's length, in seconds.
    pub seconds: Option<u32>,
    /// The video's size, width by height.
    pub size: Option<String>,
    /// Why it failed.
    pub error: Option<JobError>,
    /// Where the provider serves the videos it made, if it names them.
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
            progress: None,
            created_at: None,
            completed_at: None,
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

/// A page of jobs.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct Jobs {
    /// The jobs, in the order asked.
    pub jobs: Vec<Job>,
    /// Whether more follow the last.
    pub has_more: bool,
    /// Its fields the kind has no place for.
    pub native: Option<NativeFields>,
}

impl Jobs {
    /// A page of `jobs`.
    pub fn new(jobs: Vec<Job>, has_more: bool) -> Self {
        Self {
            jobs,
            has_more,
            native: None,
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
        // Only the video itself is at a URL; a provider refuses a still.
        let Ok(Input::Content {
            id,
            variant: Variant::Video,
            url: None,
        }) = cx.input::<Input>()
        else {
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

/// Sends a job's follow-up (`Retrieve`, `Content`, `Remix`, `Delete`, and a
/// `List` after it) to the provider that made the job, which only the job's
/// id says: by its
/// spelling (OpenAI's start `video_`, Veo's `veo_`), or, when several
/// providers spell their ids alike or an id does not match its maker's
/// spelling, by the provider's name it wraps them in on the way out,
/// `<provider>~<id>`, and unwraps on the way in. An id
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
        let found = followed(input).and_then(|id| {
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
        Ok(None)
    }

    async fn on_provider(&self, px: &mut ProviderContext) -> Result<(), Fault> {
        if px.is_kind::<Input>()
            && let Some(maker) = self.makers.iter().find(|m| m.internal == px.provider())
        {
            let maker = maker.clone();
            px.filter(move |output: &mut Output| {
                wrap_ids(output, &maker);
                Ok(())
            })?;
        }
        Ok(())
    }
}

/// The job an operation follows: the one it is on, or the one a page
/// starts after.
fn followed(input: &Input) -> Option<&str> {
    match input {
        Input::List { after, .. } => after.as_deref(),
        _ => input.job_id(),
    }
}

fn set_job_id(input: &mut Input, inner: &str) {
    match input {
        Input::Remix { id, .. }
        | Input::Retrieve { id, .. }
        | Input::Content { id, .. }
        | Input::Delete { id, .. }
        | Input::List {
            after: Some(id), ..
        } => *id = inner.to_owned(),
        _ => {}
    }
}

fn wrap_ids(output: &mut Output, maker: &Maker) {
    let wrap = |id: &mut String| {
        if maker.shared || !id.starts_with(&maker.spelling) {
            *id = format!("{}{SEPARATOR}{id}", maker.provider);
        }
    };
    match output {
        Output::Job(job) => wrap(&mut job.id),
        Output::Jobs(jobs) => jobs.jobs.iter_mut().for_each(|job| wrap(&mut job.id)),
        Output::Deleted(id) => wrap(id),
        _ => {}
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
        let mut deleted = Output::Deleted("video_1".into());
        wrap_ids(&mut deleted, &jobs().makers[0]);
        assert_eq!(deleted, Output::Deleted("east~video_1".into()));
        let mut input = Input::Retrieve {
            id: "east~video_1".into(),
        };
        set_job_id(&mut input, "video_1");
        assert_eq!(input.job_id(), Some("video_1"));
    }

    #[test]
    fn opaque_ids_and_shared_spellings_are_wrapped_in_jobs_and_pages() {
        let jobs = jobs();
        let unique = &jobs.makers[2];
        let mut page = Output::Jobs(Jobs::new(
            vec![
                Job::new("opaque", Status::Queued),
                Job::new("veo_123", Status::Queued),
            ],
            false,
        ));
        wrap_ids(&mut page, unique);
        let Output::Jobs(page) = page else {
            unreachable!()
        };
        assert_eq!(page.jobs[0].id, "gemini~opaque");
        assert_eq!(page.jobs[1].id, "veo_123");
        let mut job = Output::Job(Job::new("video_123", Status::Queued));
        wrap_ids(&mut job, &jobs.makers[0]);
        let Output::Job(job) = job else {
            unreachable!()
        };
        assert_eq!(job.id, "east~video_123");
        let mut deleted = Output::Deleted("opaque".into());
        wrap_ids(&mut deleted, unique);
        assert_eq!(deleted, Output::Deleted("gemini~opaque".into()));
        for input in [
            Input::Retrieve {
                id: page.jobs[0].id.clone(),
            },
            Input::Content {
                id: page.jobs[0].id.clone(),
                variant: Variant::Video,
                url: None,
            },
            Input::List {
                after: Some(page.jobs[0].id.clone()),
                limit: None,
                order: None,
            },
        ] {
            let id = followed(&input).expect("followed job id");
            let (maker, inner) = jobs
                .maker(id, Some("east:openai-videos"))
                .expect("job owner");
            assert_eq!((maker.provider.as_str(), inner), ("gemini", "opaque"));
            let inner = inner.to_owned();
            let mut input = input;
            set_job_id(&mut input, &inner);
            assert_eq!(followed(&input), Some("opaque"));
        }
    }

    /// Regression: a page's `after` kept the wrapping its id was given out
    /// in, and went to whichever provider the router picked.
    #[test]
    fn a_page_after_a_wrapped_job_goes_to_its_maker_unwrapped() {
        let mut input = Input::List {
            after: Some("west~video_1".into()),
            limit: None,
            order: None,
        };
        let after = followed(&input).map(str::to_owned);
        assert_eq!(after.as_deref(), Some("west~video_1"));
        let found = after
            .as_deref()
            .and_then(|id| maker(id, Some("east:openai-videos")));
        assert_eq!(found, Some(("west".to_owned(), "video_1".to_owned())));
        set_job_id(&mut input, "video_1");
        assert_eq!(followed(&input), Some("video_1"));
        let first = Input::List {
            after: None,
            limit: None,
            order: None,
        };
        assert_eq!(followed(&first), None);
    }
}
