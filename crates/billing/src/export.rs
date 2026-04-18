use async_trait::async_trait;
use common::billing::BillingRecord;
use common::error::PlatformError;
use std::sync::Arc;
use std::time::Duration;
use storage::Store;

#[async_trait]
pub trait BillingExporter: Send + Sync {
    async fn export_batch(&self, records: &[BillingRecord]) -> Result<(), PlatformError>;
}

pub struct S3Exporter {
    pub bucket: String,
    pub prefix: String,
    pub endpoint: String,
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
    pub region: String,
}

impl S3Exporter {
    pub fn new(
        bucket: String,
        prefix: String,
        endpoint: String,
        access_key: Option<String>,
        secret_key: Option<String>,
        region: String,
    ) -> Self {
        Self {
            bucket,
            prefix,
            endpoint,
            access_key,
            secret_key,
            region,
        }
    }

    pub fn with_creds(
        bucket: String,
        prefix: String,
        endpoint: String,
        access_key: String,
        secret_key: String,
        region: String,
    ) -> Self {
        Self::new(
            bucket,
            prefix,
            endpoint,
            Some(access_key),
            Some(secret_key),
            region,
        )
    }
}

#[async_trait]
impl BillingExporter for S3Exporter {
    async fn export_batch(&self, records: &[BillingRecord]) -> Result<(), PlatformError> {
        if records.is_empty() {
            return Ok(());
        }

        let mut body = Vec::new();
        for record in records {
            let line = serde_json::to_string(record).map_err(PlatformError::storage_source)?;
            body.extend(line.as_bytes());
            body.push(b'\n');
        }

        let first_ts = records.first().map(|r| r.timestamp_ms).unwrap_or(0);
        let key = format!("{}/{}/{}.ndjson", self.prefix, records[0].node_id, first_ts);

        let url = format!(
            "{}/{}/{}",
            self.endpoint.trim_end_matches('/'),
            self.bucket,
            key
        );

        let client = reqwest::Client::new();
        let mut request = client.put(&url);

        if let (Some(key), Some(_secret)) = (&self.access_key, &self.secret_key) {
            let timestamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
            let date = &timestamp[..8];
            let scope = format!("{}/{}/s3/aws4_request", date, self.region);

            request = request
                .header("x-amz-date", &timestamp)
                .header("x-amz-content-sha256", "UNSIGNED-PAYLOAD")
                .header("Authorization", format!(
                    "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature=placeholder",
                    key, scope
                ));
        }

        request = request.header("Content-Type", "application/x-ndjson");

        let response = request
            .body(body)
            .send()
            .await
            .map_err(|e| PlatformError::external(format!("S3 export failed: {}", e)))?;

        if response.status().is_success() || response.status().as_u16() == 307 {
            tracing::info!(key = %key, records = records.len(), "billing batch exported to S3");
            Ok(())
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            Err(PlatformError::external(format!(
                "S3 export failed: {} - {}",
                status, body
            )))
        }
    }
}

pub struct FileExporter {
    pub dir: std::path::PathBuf,
}

impl FileExporter {
    pub fn new(dir: std::path::PathBuf) -> Self {
        Self { dir }
    }
}

#[async_trait]
impl BillingExporter for FileExporter {
    async fn export_batch(&self, records: &[BillingRecord]) -> Result<(), PlatformError> {
        if records.is_empty() {
            return Ok(());
        }

        let first_ts = records.first().map(|r| r.timestamp_ms).unwrap_or(0);
        let node_id = records
            .first()
            .map(|r| r.node_id.clone())
            .unwrap_or_default();
        let path = self
            .dir
            .join(format!("billing_{}_{}.ndjson", node_id, first_ts));

        let mut body = String::new();
        for record in records {
            let line = serde_json::to_string(record).map_err(PlatformError::storage_source)?;
            body.push_str(&line);
            body.push('\n');
        }

        tokio::fs::create_dir_all(&self.dir)
            .await
            .map_err(PlatformError::storage_source)?;

        tokio::fs::write(&path, body.as_bytes())
            .await
            .map_err(PlatformError::storage_source)?;

        tracing::info!(
            path = %path.display(),
            records = records.len(),
            "billing batch exported"
        );
        Ok(())
    }
}

pub fn start_export_loop(store: Store, exporter: Arc<dyn BillingExporter>, interval: Duration) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        loop {
            tick.tick().await;

            match store.read_unexported_billing_records(10_000) {
                Ok(records) if records.is_empty() => {}
                Ok(records) => {
                    let count = records.len();
                    let last_seq = records.last().map(|r| r.seq).unwrap_or(0);

                    match exporter.export_batch(&records).await {
                        Ok(()) => {
                            store.set_billing_export_watermark(last_seq).ok();
                            tracing::info!(count, last_seq, "billing export complete");
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "billing export failed — will retry next tick");
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "failed to read billing records for export");
                }
            }
        }
    });
}
