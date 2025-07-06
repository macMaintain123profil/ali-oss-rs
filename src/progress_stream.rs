use std::pin::Pin;
use std::task::{Context, Poll};
use std::sync::Arc;

use tokio::io::{AsyncRead, ReadBuf};
use bytes::Bytes;
use futures::{Stream};
use tokio_util::io::ReaderStream;
use crate::{Result, ClientBuilder};
use futures::future::join_all;
use std::{ops::Range};
use tokio::sync::Semaphore;
use tokio::task;


use crate::{
    Client,
    multipart::MultipartUploadsOperations,
    multipart_common::{
        UploadPartRequest, 
        CompleteMultipartUploadRequest, 
        CompleteMultipartUploadResult, 
        InitiateMultipartUploadOptions,
        UploadPartResult,
        build_upload_part_request
    },
    object_common::{PutObjectOptions, PutObjectResult, build_put_object_request}, 
    request::{RequestBody}
};
use std::path::Path;

pub type ProgressCallback = Arc<dyn Fn(u64, u64) + Send + Sync>;

pub struct ProgressStream<R> {
    inner: ReaderStream<R>,
    callback: ProgressCallback,
    uploaded: u64,
    total: u64,
}

impl<R> ProgressStream<R>
where
    R: AsyncRead + Unpin,
{
    pub fn new(reader: R, total: u64, callback: ProgressCallback) -> Self {
        ProgressStream {
            inner: ReaderStream::new(reader),
            callback,
            uploaded: 0,
            total,
        }
    }
}

impl<R> Stream for ProgressStream<R>
where
    R: AsyncRead + Unpin,
{
    type Item = std::result::Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let poll = Pin::new(&mut self.inner).poll_next(cx);

        if let Poll::Ready(Some(Ok(ref bytes))) = &poll {
            self.uploaded += bytes.len() as u64;
            (self.callback)(self.uploaded, self.total);
        }

        poll
    }
}

impl Client {
    pub async fn put_object_from_file_with_progress<S1, S2, P>(&self, 
        bucket_name: S1, 
        object_key: S2, 
        file_path: P, 
        options: Option<PutObjectOptions>,
        progress_callback: Option<ProgressCallback>) -> Result<PutObjectResult>
    where
        S1: AsRef<str> + Send,
        S2: AsRef<str> + Send,
        P: AsRef<Path> + Send,
    {
        let bucket_name = bucket_name.as_ref();
        let object_key = object_key.as_ref();

        let object_key = object_key.strip_prefix("/").unwrap_or(object_key);
        let object_key = object_key.strip_suffix("/").unwrap_or(object_key);

        let file_path = file_path.as_ref();

        let with_callback = if let Some(opt) = &options { opt.callback.is_some() } else { false };

        let mut request = build_put_object_request(bucket_name, object_key, RequestBody::File(file_path.to_path_buf(), None), &options)?;
        request.progress_callback = progress_callback;

        let (headers, content) = self.do_request::<String>(request).await?;

        if with_callback {
            Ok(PutObjectResult::CallbackResponse(content))
        } else {
            Ok(PutObjectResult::ApiResponse(headers.into()))
        }
    }

    pub async fn put_object_from_file_with_multipart_progress(self: Arc<Self>, 
        bucket_name: String, 
        object_key: String, 
        file_path: String, 
        part_count: u64,
        options: Option<InitiateMultipartUploadOptions>,
        progress_callback: Option<ProgressCallback>) -> Result<CompleteMultipartUploadResult>
    {
        // 1、对文件进行分片
        let meta = std::fs::metadata(Path::new(&file_path)).unwrap();
        let file_size = meta.len();
        // <MinSizeAllowed>102400</MinSizeAllowed>
        // 分片大小 1 MB (兆字节) = 1024 KB(千字节/兆字节), 1 KB= 1024 Byte(字节)
        // 最小分为3片，每片最大5MB
        let max_part_size = 5 * 1024 * 1024; // 5MB
        let min_part_size = 100 * 1024; // 100KB
        let real_part_size = file_size.div_ceil(part_count);
        let slice_len = if real_part_size < min_part_size {
            // 如果分片大小<=5MB，则每片大小为文件大小/分片的数量
            min_part_size
        } else if real_part_size > max_part_size {
            max_part_size
        } else {
            real_part_size
        };
        let mut ranges = vec![];
        let mut c = 0;
        // 将文件分片计算出来
        loop {
            let end = (c + 1) * slice_len;
            let r = Range {
                start: c * slice_len,
                end: end.min(meta.len()),
            };

            ranges.push(r);

            if end >= meta.len() {
                break;
            }

            c += 1;
        }
        // 2、初始化分片
        let init_response = self.initiate_multipart_uploads(
            bucket_name.clone(), 
            object_key.clone(), 
            options).await?;

        let upload_id = init_response.upload_id.clone();

        // 3、开始并行分片上传
        let max_concurrent = 5; // 自定义最大并发数
        let semaphore = Arc::new(Semaphore::new(max_concurrent));

        let mut tasks = vec![];
        for (i, rng) in ranges.iter().enumerate() {
        
            let rng = rng.clone();
            let upload_id = upload_id.clone();
            let permit = semaphore.clone().acquire_owned().await.unwrap();
            let file_path_x = file_path.clone();
            let bucket_name_x = bucket_name.clone();
            let object_key_x = object_key.clone();
            let progress_callback_x = progress_callback.clone();
            let self_x = self.clone();
            let task = task::spawn(async move {
                let _permit = permit; // 保证生命周期
                let upload_data = UploadPartRequest {
                    part_number: (i + 1) as u32,
                    upload_id,
                };
                let resp = self_x
                    .upload_part_from_file_with_progress(
                        &bucket_name_x, 
                        &object_key_x, 
                        &file_path_x, 
                        rng, 
                        upload_data,
                        progress_callback_x)
                    .await;

                match resp {
                    Ok(r) => Ok(((i + 1) as u32, r.etag)),
                    Err(e) => Err((i + 1, e)),
                }
            });

            tasks.push(task);
        }
        // 等待上传完成
        let results = join_all(tasks).await;
        // 对上传完成后的数据进行组装
        let mut upload_results = vec![];
        for res in results {
            match res {
                Ok(Ok((part_number, etag))) => {
                    upload_results.push((part_number, etag));
                }
                Ok(Err((part_number, err))) => {
                    eprintln!("Upload failed at part {}: {:?}", part_number, err);
                    // 可以记录或重试
                }
                Err(join_err) => {
                    eprintln!("Join error: {:?}", join_err);
                }
            }
        }
        // 4、合并分片
        let comp_data = CompleteMultipartUploadRequest {
            upload_id,
            parts: upload_results,
        };
        let complete_response = self.complete_multipart_uploads(
            bucket_name.clone(), 
            object_key.clone(), 
            comp_data, 
            None).await?;
        // 返回结果
        Ok(complete_response)
    }

    async fn upload_part_from_file_with_progress<S1, S2, P>(
        &self,
        bucket_name: S1,
        object_key: S2,
        file_path: P,
        range: Range<u64>,
        params: UploadPartRequest,
        progress_callback: Option<ProgressCallback>,
    ) -> Result<UploadPartResult>
    where
        S1: AsRef<str> + Send,
        S2: AsRef<str> + Send,
        P: AsRef<Path> + Send,
    {
        let mut request = build_upload_part_request(
            bucket_name.as_ref(),
            object_key.as_ref(),
            RequestBody::File(file_path.as_ref().to_path_buf(), Some(range)),
            params,
        )?;
        request.progress_callback = progress_callback;

        let (headers, _) = self.do_request::<()>(request).await?;

        Ok(headers.into())
    }
}


impl ClientBuilder {
    /// `endpoint` could be: `oss-cn-hangzhou.aliyuncs.com` without scheme part.
    /// or you can include scheme part in the `endpoint`: `https://oss-cn-hangzhou.aliyuncs.com`.
    /// if no scheme specified, use `https` by default.
    ///
    /// # Examples
    //
    /// ```
    /// let client = ali_oss_rs::ClientBuilder::new(
    ///     "your access key id",
    ///     "your acess key secret",
    ///     "oss-cn-hangzhou.aliyuncs.com"
    /// ).build();
    /// ```
    pub fn new_with_auto_region<S1, S2, S3>(access_key_id: S1, access_key_secret: S2, endpoint: S3) -> Self
    where
        S1: AsRef<str>,
        S2: AsRef<str>,
        S3: AsRef<str>,
    {
        if let Some(region) = parse_region_from_endpoint(endpoint.as_ref()) {
            // 不设置region会没法上传，报错Invalid signing region in Authorization header.
            Self {
                access_key_id: access_key_id.as_ref().to_string(),
                access_key_secret: access_key_secret.as_ref().to_string(),
                endpoint: endpoint.as_ref().to_string(),
                region: Some(region),
                ..Default::default()
            }
        } else {
            Self {
                access_key_id: access_key_id.as_ref().to_string(),
                access_key_secret: access_key_secret.as_ref().to_string(),
                endpoint: endpoint.as_ref().to_string(),
                ..Default::default()
            }
        }
    }

    
}

fn parse_region_from_endpoint(endpoint: &str) -> Option<String> {
    // oss-cn-hongkong.aliyuncs.com -> cn-hongkong
    let endpoint = endpoint.trim_start_matches("https://").trim_start_matches("http://");
    if endpoint.starts_with("oss-") {
        endpoint
            .strip_prefix("oss-")
            .and_then(|s| s.split('.').next())
            .map(|region| region.to_string())
    } else {
        None
    }
}