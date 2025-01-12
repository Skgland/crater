use crate::prelude::*;
use crate::report::ReportWriter;
use crate::results::EncodingType;
use aws_sdk_s3::Client as S3Client;
use mime::Mime;
use std::borrow::Cow;
use std::fmt::{self, Display};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use url::{Host, Url};

#[derive(Debug, thiserror::Error)]
pub enum S3Error {
    #[error("bad S3 url: {0}")]
    BadUrl(String),
}

#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub struct S3Prefix {
    pub bucket: String,
    pub prefix: PathBuf,
}

impl FromStr for S3Prefix {
    type Err = ::anyhow::Error;

    fn from_str(url: &str) -> Fallible<S3Prefix> {
        let parsed = Url::parse(url).with_context(|| S3Error::BadUrl(url.into()))?;

        if parsed.scheme() != "s3"
            || parsed.username() != ""
            || parsed.password().is_some()
            || parsed.port().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(S3Error::BadUrl(url.into()).into());
        }

        let bucket = if let Some(Host::Domain(host)) = parsed.host() {
            host.to_string()
        } else {
            return Err(S3Error::BadUrl(url.into()).into());
        };

        Ok(S3Prefix {
            bucket,
            prefix: parsed
                .path()
                .get(1..)
                .map(PathBuf::from)
                .unwrap_or_default(),
        })
    }
}

pub struct S3Writer {
    bucket: String,
    prefix: String,
    client: S3Client,
    runtime: tokio::runtime::Runtime,
}

impl S3Writer {
    pub fn create(client: S3Client, bucket: String, prefix: String) -> Fallible<S3Writer> {
        Ok(S3Writer {
            bucket,
            prefix,
            client,
            runtime: tokio::runtime::Runtime::new()?,
        })
    }
}

impl ReportWriter for S3Writer {
    fn write_bytes<P: AsRef<Path>>(
        &self,
        path: P,
        body: &[u8],
        mime: &Mime,
        encoding_type: EncodingType,
    ) -> Fallible<()> {
        // At least 50 MB, then use a multipart upload...
        if body.len() >= 50 * 1024 * 1024 {
            let mut request = self
                .client
                .create_multipart_upload()
                .acl(aws_sdk_s3::types::ObjectCannedAcl::PublicRead)
                .key(format!(
                    "{}/{}",
                    self.prefix,
                    path.as_ref().to_str().unwrap()
                ))
                .content_type(mime.to_string())
                .bucket(self.bucket.clone());
            match encoding_type {
                EncodingType::Plain => {}
                EncodingType::Gzip => {
                    request = request.content_encoding("gzip");
                }
            }
            let upload = match self.runtime.block_on(request.send()) {
                Ok(u) => u,
                Err(e) => {
                    bail!("Failed to upload to {:?}: {:?}", path.as_ref(), e);
                }
            };

            let chunk_size = 20 * 1024 * 1024;
            let mut part = 1;
            let mut start = 0;
            let mut parts = aws_sdk_s3::types::CompletedMultipartUpload::builder();
            while start < body.len() {
                let chunk = &body[start..std::cmp::min(start + chunk_size, body.len())];
                let chunk = bytes::Bytes::copy_from_slice(chunk);

                let request = self
                    .client
                    .upload_part()
                    .part_number(part)
                    .body(chunk.into())
                    .upload_id(upload.upload_id().unwrap())
                    .key(upload.key().unwrap())
                    .bucket(self.bucket.clone());
                match self.runtime.block_on(request.send()) {
                    Ok(p) => {
                        parts = parts.parts(
                            aws_sdk_s3::types::CompletedPart::builder()
                                .e_tag(p.e_tag.clone().unwrap())
                                .part_number(part)
                                .build(),
                        )
                    }
                    Err(e) => {
                        bail!("Failed to upload to {:?}: {:?}", path.as_ref(), e);
                    }
                };

                start += chunk_size;
                part += 1;
            }

            let request = self
                .client
                .complete_multipart_upload()
                .multipart_upload(parts.build())
                .upload_id(upload.upload_id().unwrap())
                .key(upload.key().unwrap())
                .bucket(self.bucket.clone());
            match self.runtime.block_on(request.send()) {
                Ok(_) => (),
                Err(e) => {
                    bail!("Failed to upload to {:?}: {:?}", path.as_ref(), e);
                }
            };

            Ok(())
        } else {
            let mut request = self
                .client
                .put_object()
                .body(aws_sdk_s3::primitives::ByteStream::from(
                    bytes::Bytes::copy_from_slice(body),
                ))
                .acl(aws_sdk_s3::types::ObjectCannedAcl::PublicRead)
                .key(format!(
                    "{}/{}",
                    self.prefix,
                    path.as_ref().to_str().unwrap()
                ))
                .content_type(mime.to_string())
                .bucket(self.bucket.clone());
            match encoding_type {
                EncodingType::Plain => {}
                EncodingType::Gzip => {
                    request = request.content_encoding("gzip");
                }
            }
            match self.runtime.block_on(request.send()) {
                Ok(_) => Ok(()),
                Err(e) => {
                    bail!("Failed to upload to {:?}: {:?}", path.as_ref(), e);
                }
            }
        }
    }

    fn write_string<P: AsRef<Path>>(&self, path: P, s: Cow<str>, mime: &Mime) -> Fallible<()> {
        self.write_bytes(path, s.as_bytes(), mime, EncodingType::Plain)
    }
}

impl Display for S3Prefix {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        format_args!("s3://{}/{}", self.bucket, self.prefix.display()).fmt(f)
    }
}

impl Display for S3Writer {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.prefix.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::S3Prefix;
    use std::str::FromStr;

    #[test]
    fn test_parse_s3prefix() {
        assert_eq!(
            S3Prefix::from_str("s3://bucket-name").unwrap(),
            S3Prefix {
                bucket: "bucket-name".into(),
                prefix: "".into(),
            }
        );
        assert_eq!(
            S3Prefix::from_str("s3://bucket-name/path/prefix").unwrap(),
            S3Prefix {
                bucket: "bucket-name".into(),
                prefix: "path/prefix".into(),
            }
        );

        for bad in &[
            "https://example.com",
            "s3://user:pass@bucket/path/prefix",
            "s3://bucket:80",
            "s3://bucket/path/prefix?query#fragment",
        ] {
            assert!(S3Prefix::from_str(bad).is_err(), "valid bad url: {bad}");
        }
    }
}
