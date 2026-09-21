use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::time::{timeout_at, Instant};
use uuid::Uuid;

pub const FRAME_LIMIT: usize = 32 * 1024;
const PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, PartialEq, Eq)]
pub struct BindingSecret([u8; 32]);

impl<'de> Deserialize<'de> for BindingSecret {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        if encoded.len() != 64
            || !encoded
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(serde::de::Error::custom(
                "binding must be 64 lowercase hexadecimal characters",
            ));
        }

        let mut decoded = [0_u8; 32];
        for (index, pair) in encoded.as_bytes().chunks_exact(2).enumerate() {
            decoded[index] = (hex_nibble(pair[0]).expect("validated hex") << 4)
                | hex_nibble(pair[1]).expect("validated hex");
        }
        Ok(Self(decoded))
    }
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Bind {
        version: u32,
        thread_id: Uuid,
        binding: BindingSecret,
    },
    Inspect {
        version: u32,
        thread_id: Uuid,
        binding: BindingSecret,
    },
    Queue {
        version: u32,
        thread_id: Uuid,
        binding: BindingSecret,
        attempt_id: Uuid,
        text: String,
    },
}

impl Request {
    fn validate(&self) -> Result<()> {
        let version = match self {
            Self::Bind { version, .. }
            | Self::Inspect { version, .. }
            | Self::Queue { version, .. } => *version,
        };
        if version != PROTOCOL_VERSION {
            bail!("unsupported relay protocol version");
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Submission {
    NotStarted,
    Uncertain,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidRequest,
    Unauthorized,
    Conflict,
    ThreadMismatch,
    Unavailable,
    Timeout,
    UpstreamError,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThreadState {
    Idle,
    Active,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Response {
    version: u32,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<ThreadState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    receipt: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorCode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    submission: Option<Submission>,
}

impl Response {
    pub fn error(error: ErrorCode, submission: Submission) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            ok: false,
            state: None,
            receipt: None,
            error: Some(error),
            submission: Some(submission),
        }
    }

    #[cfg(test)]
    fn upstream_error(submission: Submission, _source: &anyhow::Error) -> Self {
        Self::error(ErrorCode::UpstreamError, submission)
    }
}

fn decode_payload(payload: &[u8]) -> Result<Request> {
    if payload.is_empty() || payload.len() > FRAME_LIMIT {
        bail!("relay frame length is out of bounds");
    }
    let request: Request = serde_json::from_slice(payload).context("invalid relay request")?;
    request.validate()?;
    Ok(request)
}

pub async fn read_frame(stream: &mut UnixStream, deadline: Instant) -> Result<Request> {
    let mut length = [0_u8; 4];
    timeout_at(deadline, stream.read_exact(&mut length))
        .await
        .context("relay read deadline exceeded")?
        .context("reading relay frame length")?;
    let length = u32::from_be_bytes(length) as usize;
    if !(1..=FRAME_LIMIT).contains(&length) {
        bail!("relay frame length is out of bounds");
    }

    let mut payload = vec![0_u8; length];
    timeout_at(deadline, stream.read_exact(&mut payload))
        .await
        .context("relay read deadline exceeded")?
        .context("reading relay frame body")?;
    decode_payload(&payload)
}

pub async fn write_frame(
    stream: &mut UnixStream,
    response: &Response,
    deadline: Instant,
) -> Result<()> {
    let payload = serde_json::to_vec(response).context("encoding relay response")?;
    if payload.is_empty() || payload.len() > FRAME_LIMIT {
        bail!("relay response length is out of bounds");
    }

    let length = u32::try_from(payload.len())?.to_be_bytes();
    timeout_at(deadline, async {
        stream.write_all(&length).await?;
        stream.write_all(&payload).await?;
        stream.flush().await
    })
    .await
    .context("relay write deadline exceeded")?
    .context("writing relay frame")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixStream;
    use tokio::time::Instant;

    const THREAD: &str = "11111111-1111-4111-8111-111111111111";
    const ATTEMPT: &str = "22222222-2222-4222-8222-222222222222";
    const BINDING: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn lam_relay_accepts_each_strict_version_one_operation() {
        let bind = format!(
            r#"{{"version":1,"operation":"bind","thread_id":"{THREAD}","binding":"{BINDING}"}}"#
        );
        let inspect = format!(
            r#"{{"version":1,"operation":"inspect","thread_id":"{THREAD}","binding":"{BINDING}"}}"#
        );
        let queue = format!(
            r#"{{"version":1,"operation":"queue","thread_id":"{THREAD}","binding":"{BINDING}","attempt_id":"{ATTEMPT}","text":"hello"}}"#
        );

        assert!(matches!(
            decode_payload(bind.as_bytes()),
            Ok(Request::Bind { .. })
        ));
        assert!(matches!(
            decode_payload(inspect.as_bytes()),
            Ok(Request::Inspect { .. })
        ));
        assert!(matches!(
            decode_payload(queue.as_bytes()),
            Ok(Request::Queue { .. })
        ));
    }

    #[test]
    fn lam_relay_rejects_unsupported_versions_unknown_fields_and_bad_values() {
        let version = format!(
            r#"{{"version":2,"operation":"inspect","thread_id":"{THREAD}","binding":"{BINDING}"}}"#
        );
        let extra = format!(
            r#"{{"version":1,"operation":"inspect","thread_id":"{THREAD}","binding":"{BINDING}","extra":true}}"#
        );
        let uppercase_binding = format!(
            r#"{{"version":1,"operation":"inspect","thread_id":"{THREAD}","binding":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}}"#
        );
        let bad_attempt = format!(
            r#"{{"version":1,"operation":"queue","thread_id":"{THREAD}","binding":"{BINDING}","attempt_id":"nope","text":"hello"}}"#
        );

        for invalid in [version, extra, uppercase_binding, bad_attempt] {
            assert!(decode_payload(invalid.as_bytes()).is_err());
        }
        assert!(decode_payload(&vec![b'x'; FRAME_LIMIT + 1]).is_err());
        assert!(decode_payload(&[]).is_err());
    }

    #[tokio::test]
    async fn lam_relay_read_uses_one_absolute_deadline() {
        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        writer.write_all(&16_u32.to_be_bytes()).await.unwrap();
        writer.write_all(b"{").await.unwrap();

        let started = Instant::now();
        let deadline = started + Duration::from_millis(40);
        assert!(read_frame(&mut reader, deadline).await.is_err());
        assert!(started.elapsed() < Duration::from_millis(200));
    }

    #[tokio::test]
    async fn lam_relay_write_emits_a_bounded_length_prefixed_response() {
        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        let response = Response::error(ErrorCode::Unauthorized, Submission::NotStarted);
        write_frame(
            &mut writer,
            &response,
            Instant::now() + Duration::from_secs(1),
        )
        .await
        .unwrap();

        let mut length = [0_u8; 4];
        reader.read_exact(&mut length).await.unwrap();
        let length = u32::from_be_bytes(length) as usize;
        assert!((1..=FRAME_LIMIT).contains(&length));
        let mut payload = vec![0_u8; length];
        reader.read_exact(&mut payload).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&payload).unwrap(),
            serde_json::json!({
                "version": 1,
                "ok": false,
                "error": "unauthorized",
                "submission": "not_started"
            })
        );
    }

    #[test]
    fn lam_relay_fixed_errors_never_serialize_provider_text() {
        let response = Response::upstream_error(
            Submission::Uncertain,
            &anyhow::anyhow!("provider leaked private message"),
        );
        let json = serde_json::to_string(&response).unwrap();

        assert!(json.contains("upstream_error"));
        assert!(json.contains("uncertain"));
        assert!(!json.contains("provider"));
        assert!(!json.contains("private message"));
    }
}
