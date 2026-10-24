//! Configuration, base64 text audio, and Soniox controls.

use respan_gateway_core::Fault;
use respan_gateway_http::{Encoder, bytes::Bytes};
use respan_gateway_realtime::{Audio, AudioFormat, Input, Purpose, Session, Turns};
use serde_json::{Value, json};

use super::NAME;
use crate::audio::Convert;
use crate::foreign;
use crate::json::{self, merge, own, untranslatable_input};

const AUDIO: AudioFormat = AudioFormat::pcm16(16_000);

#[derive(Debug)]
pub(super) struct Client {
    model: Option<String>,
    opened: bool,
    audio: Option<AudioFormat>,
    ended: bool,
}

impl Client {
    pub(super) fn new(model: Option<String>) -> Self {
        Self {
            model,
            opened: false,
            audio: None,
            ended: false,
        }
    }

    pub(super) fn read(&mut self, raw: Bytes, out: &mut Vec<Input>) -> Result<(), Fault> {
        if self.ended {
            return Err(Fault::bad_request("Soniox audio has already ended"));
        }
        if !self.opened {
            let mut config: Value = serde_json::from_slice(&raw).map_err(|_| {
                Fault::bad_request("Soniox's first frame must be its JSON configuration")
            })?;
            let fields = config
                .as_object_mut()
                .ok_or_else(|| Fault::bad_request("Soniox configuration must be an object"))?;
            if fields.contains_key("api_key") {
                return Err(Fault::bad_request(
                    "authenticate Soniox at the WebSocket handshake, not with api_key in its configuration",
                ));
            }
            let model = fields.shift_remove("model");
            let mut session = Session::default();
            session.model = match model {
                Some(Value::String(model)) if !model.is_empty() => Some(model),
                Some(_) => {
                    return Err(Fault::bad_request("Soniox model must be a nonempty string"));
                }
                None => self.model.take(),
            };
            if !fields.get("audio_format").is_some_and(Value::is_string) {
                return Err(Fault::bad_request(
                    "Soniox configuration requires audio_format",
                ));
            }
            self.audio = pcm(&config);
            session.input_audio = self.audio;
            if self.audio.is_some() {
                for key in ["audio_format", "sample_rate", "num_channels"] {
                    config.as_object_mut().expect("object").shift_remove(key);
                }
            }
            session.purpose = Some(Purpose::Transcription);
            session.input_transcription = Some(true);
            session.native = json::native(NAME, config);
            self.opened = true;
            out.push(Input::Session(session));
            return Ok(());
        }
        if raw.is_empty() {
            self.ended = true;
            out.push(Input::native(NAME, ""));
            return Ok(());
        }
        let text = std::str::from_utf8(&raw)
            .map_err(|_| Fault::bad_request("Soniox audio must be base64 text"))?;
        if text.trim_start().starts_with('{') {
            let control: Value = serde_json::from_str(text)
                .map_err(|_| Fault::bad_request("Soniox control is not JSON"))?;
            if control.get("api_key").is_some() {
                return Err(Fault::bad_request("Soniox controls cannot contain api_key"));
            }
            return match control["type"].as_str() {
                Some("finalize" | "keepalive") => {
                    out.push(Input::native(NAME, control));
                    Ok(())
                }
                _ => Err(Fault::bad_request(
                    "Soniox configuration is sent once; controls are finalize or keepalive",
                )),
            };
        }
        let value = Value::String(text.to_owned());
        let data = json::unbase64(&value)
            .ok_or_else(|| Fault::bad_request("Soniox audio must be base64 text"))?;
        out.push(match self.audio {
            Some(format) => Input::Audio {
                audio: Audio { data, format },
                native: None,
            },
            None => Input::native(NAME, value),
        });
        Ok(())
    }
}

fn pcm(config: &Value) -> Option<AudioFormat> {
    if config["audio_format"] != "pcm_s16le" || config["num_channels"] != 1 {
        return None;
    }
    let rate = u32::try_from(config["sample_rate"].as_u64()?).ok()?;
    (rate > 0).then_some(AudioFormat::pcm16(rate))
}

#[derive(Debug)]
pub(super) struct Provider {
    model: Option<String>,
    config: Option<Value>,
    audio: Option<AudioFormat>,
    convert: Convert,
    ended: bool,
    heard: bool,
}

impl Provider {
    pub(super) fn new(model: Option<String>) -> Self {
        Self {
            model,
            config: None,
            audio: None,
            convert: Convert::default(),
            ended: false,
            heard: false,
        }
    }

    pub(super) fn write(&mut self, event: &Input, out: &mut Vec<Bytes>) -> Result<(), Fault> {
        let refuse = |what| untranslatable_input(NAME, what);
        if self.ended {
            return Err(refuse("an event after the audio ended"));
        }
        let event = foreign::admit(NAME, event)?;
        if let Input::Session(session) = &event {
            return self.session(session, out);
        }
        if self.config.is_none() {
            self.session(&Session::default(), out)?;
        }
        match &event {
            Input::Audio { audio, native } => {
                if own(NAME, native, || refuse("another format's audio"))?
                    .is_some_and(|fields| !fields.is_empty())
                {
                    return Err(refuse("native audio fields"));
                }
                let format = self
                    .audio
                    .ok_or_else(|| refuse("typed audio for this native audio format"))?;
                let data = self
                    .convert
                    .to(audio, format)
                    .ok_or_else(|| refuse("audio of this format"))?;
                for chunk in data.chunks(262_144) {
                    out.push(Bytes::from(
                        json::base64(chunk).as_str().expect("base64").to_owned(),
                    ));
                }
                self.heard |= !data.is_empty();
            }
            Input::Commit { native } => {
                let mut control = json!({"type": "finalize"});
                merge(
                    &mut control,
                    own(NAME, native, || refuse("another format's commit"))?,
                );
                out.push(json::frame(&control)?);
                self.heard = false;
            }
            Input::Native(native) if *native.format == *NAME => {
                let value = &*native.value;
                match value {
                    Value::String(text) if text.is_empty() => {
                        self.ended = true;
                        out.push(Bytes::new());
                    }
                    Value::String(text) if json::unbase64(value).is_some() => {
                        self.heard = true;
                        out.push(Bytes::from(text.clone()));
                    }
                    Value::Object(_)
                        if matches!(value["type"].as_str(), Some("keepalive" | "finalize"))
                            && value.get("api_key").is_none() =>
                    {
                        if value["type"] == "finalize" {
                            self.heard = false;
                        }
                        out.push(json::frame(value)?);
                    }
                    _ => return Err(refuse("this Soniox control")),
                }
            }
            Input::Native(native) => foreign::native_input(NAME, native, self.heard)?,
            _ => return Err(refuse("this event")),
        }
        Ok(())
    }

    fn session(&mut self, session: &Session, out: &mut Vec<Bytes>) -> Result<(), Fault> {
        let refuse = |what| untranslatable_input(NAME, what);
        if session.purpose == Some(Purpose::Conversation)
            || session.instructions.is_some()
            || session.output.is_some()
            || session.voice.is_some()
            || session.output_audio.is_some()
            || session.tools.is_some()
            || session.tool_choice.is_some()
            || session.max_output_tokens.is_some()
            || session.input_transcription == Some(false)
        {
            return Err(refuse("a conversation or speech generation setting"));
        }
        let mut config = Value::Object(
            own(NAME, &session.native, || {
                refuse("another format's configuration")
            })?
            .cloned()
            .unwrap_or_default(),
        );
        if config.get("api_key").is_some() {
            return Err(refuse(
                "api_key in a configuration; use handshake authentication",
            ));
        }
        if let Some(audio) = session.input_audio {
            if audio.rate == 0 || audio.encoding != respan_gateway_realtime::Encoding::Pcm16 {
                return Err(refuse("this session audio format"));
            }
            config["audio_format"] = "pcm_s16le".into();
            config["sample_rate"] = audio.rate.into();
            config["num_channels"] = 1.into();
        }
        if let Some(turns) = &session.turns {
            if let Turns::Server(vad) = turns
                && (vad.silence_ms.is_some() || vad.prefix_ms.is_some() || vad.native.is_some())
            {
                return Err(refuse("these voice detection settings"));
            }
            config["enable_endpoint_detection"] = matches!(turns, Turns::Server(_)).into();
        }
        let model = self
            .model
            .as_ref()
            .or(session.model.as_ref())
            .ok_or_else(|| refuse("a session without a model"))?;
        config["model"] = model.as_str().into();
        if let Some(previous) = &self.config {
            if session.model.as_ref().is_some_and(|asked| asked != model)
                || config
                    .as_object()
                    .expect("object")
                    .iter()
                    .any(|(key, value)| previous.get(key) != Some(value))
            {
                return Err(refuse("a configuration change after the session started"));
            }
            return Ok(());
        }
        if config.get("audio_format").is_none() {
            config["audio_format"] = "pcm_s16le".into();
            config["sample_rate"] = AUDIO.rate.into();
            config["num_channels"] = 1.into();
        }
        self.audio = pcm(&config);
        self.model = Some(model.clone());
        out.push(json::frame(&config)?);
        self.config = Some(config);
        Ok(())
    }
}

impl Encoder<Input> for Provider {
    fn encode(&mut self, event: &Input, out: &mut Vec<Bytes>) -> Result<(), Fault> {
        self.write(event, out)
    }
    fn end(&mut self, end: Result<(), Fault>, out: &mut Vec<Bytes>) -> Result<(), Fault> {
        if end.is_ok() && self.config.is_some() && !self.ended {
            self.ended = true;
            out.push(Bytes::new());
        }
        end
    }
}
