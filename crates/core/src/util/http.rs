//! Shared HTTP response helpers.

use futures_util::StreamExt as _;

/// Why [`read_capped`] gave up on a response body.
#[derive(Debug, thiserror::Error)]
pub enum BodyError {
    #[error("response body exceeds {max_bytes} bytes")]
    TooLarge { max_bytes: u64 },
    #[error("response body could not be read")]
    Read(#[source] reqwest::Error),
}

impl BodyError {
    /// Strip the request URL from a transport error, like
    /// [`reqwest::Error::without_url`], for call sites that keep URLs
    /// (query strings, instance ids) out of logs.
    pub fn without_url(self) -> Self {
        match self {
            Self::Read(error) => Self::Read(error.without_url()),
            too_large @ Self::TooLarge { .. } => too_large,
        }
    }
}

/// Stream a body, bailing as soon as it would exceed `max_bytes`.
///
/// Every external response body goes through this instead of
/// `Response::text`/`Response::json`, so a misbehaving upstream can make the
/// bot buffer at most `max_bytes`, not an unbounded body.
pub async fn read_capped(
    response: reqwest::Response,
    max_bytes: u64,
) -> Result<Vec<u8>, BodyError> {
    if let Some(length) = response.content_length()
        && length > max_bytes
    {
        return Err(BodyError::TooLarge { max_bytes });
    }

    let initial_capacity = response
        .content_length()
        .map(|len| len.min(max_bytes) as usize)
        .unwrap_or(0);

    let mut stream = response.bytes_stream();
    let mut buf: Vec<u8> = Vec::with_capacity(initial_capacity);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(BodyError::Read)?;
        if (buf.len() as u64).saturating_add(chunk.len() as u64) > max_bytes {
            return Err(BodyError::TooLarge { max_bytes });
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}
