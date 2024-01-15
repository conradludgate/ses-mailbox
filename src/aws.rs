use core::time;
use std::pin::Pin;

use aws_sdk_sqs::types::DeleteMessageBatchRequestEntry;
use tokio::sync::{mpsc, oneshot};
use utils::ipc::{BoxedError, DeliveryEvent, DeliveryResult, IngestMessage};

#[tracing::instrument(skip_all)]
pub async fn ingest(queue_url: String, delivery_tx: mpsc::Sender<DeliveryEvent>) {
    let config = aws_config::load_from_env().await;
    let sqs_client = aws_sdk_sqs::Client::new(&config);
    let s3_client = aws_sdk_s3::Client::new(&config);

    tracing::info!(%queue_url, "Starting SQS mail ingestor");

    loop {
        let response = sqs_client
            .receive_message()
            .max_number_of_messages(10)
            .wait_time_seconds(20)
            .queue_url(&queue_url)
            .send()
            .await;

        let messages = match response {
            Ok(message) => message,
            Err(e) => {
                tracing::warn!("error getting messages: {e:?}");
                tokio::time::sleep(time::Duration::from_secs(600)).await;
                continue;
            }
        };

        tracing::info!("got messages");

        let mut responses = vec![];

        for message in messages.messages() {
            let Some(handle) = message.receipt_handle() else {
                tracing::warn!("missing message receipt handle");
                continue;
            };
            let Some(body) = message.body() else {
                tracing::warn!("missing message body");
                continue;
            };
            let Some(message_id) = message.body() else {
                tracing::warn!("missing message ID");
                continue;
            };

            let value = match serde_json::from_str::<SQS<S3Email>>(body) {
                Ok(message) => message,
                Err(e) => {
                    tracing::warn!("error parsing message from sqs: {e:?}");
                    continue;
                }
            };

            let action = &value.message.receipt.action;
            let response = s3_client
                .get_object()
                .bucket(&action.bucket_name)
                .key(&action.object_key)
                .send()
                .await;

            let message = match response {
                Ok(message) => message,
                Err(e) => {
                    tracing::warn!("error getting object from s3: {e:?}");
                    continue;
                }
            };

            tracing::trace!(id = ?message_id, source = ?value.message.mail.source, destination = ?value.message.mail.destination, "you got mail");

            let mut body = message.body;

            let (tx, rx) = oneshot::channel();

            delivery_tx
                .send(DeliveryEvent::Ingest {
                    message: IngestMessage {
                        sender_address: value.message.mail.source,
                        recipients: value.message.mail.destination,
                        message_data: utils::ipc::MessageData::Bytes(Box::pin(
                            futures::stream::poll_fn(move |cx| {
                                Pin::new(&mut body)
                                    .poll_next(cx)
                                    .map_err(|x| Box::new(x) as BoxedError)
                            }),
                        )),
                    },
                    result_tx: tx,
                })
                .await
                .expect("mail server closed");

            let entry = DeleteMessageBatchRequestEntry::builder()
                .id(uuid::Uuid::now_v7().to_string())
                .receipt_handle(handle)
                .build()
                .unwrap();

            responses.push((rx, entry));
        }

        let mut builder = sqs_client.delete_message_batch().queue_url(&queue_url);

        for (rx, entry) in responses {
            match rx.await {
                Ok(resp) => {
                    let mut all_ok = true;
                    for resp in resp {
                        match resp {
                            DeliveryResult::Success => {}
                            DeliveryResult::PermanentFailure { code, reason } => {
                                tracing::warn!(code = ?code, "permanent ingest failure: {reason}");
                            }
                            DeliveryResult::TemporaryFailure { reason } => {
                                tracing::warn!("temporary ingest failure: {reason}");
                                all_ok = false;
                            }
                        }
                    }
                    if all_ok {
                        builder = builder.entries(entry)
                    }
                }
                Err(e) => {
                    tracing::warn!("could not ingest message: {e:?}");
                }
            }
        }

        if builder
            .get_entries()
            .as_ref()
            .is_some_and(|x| !x.is_empty())
        {
            if let Err(err) = builder.send().await {
                tracing::warn!("could not clear messages: {err:?}");
            }
        }
    }
}

#[derive(serde::Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
#[serde(bound(deserialize = "T: serde::de::DeserializeOwned"))]
struct SQS<T> {
    #[serde(rename = "Type")]
    kind: String,
    message_id: String,
    signature: String,
    signature_version: String,
    #[serde(rename = "SigningCertURL")]
    signing_cert_url: String,
    subject: String,
    timestamp: String,
    topic_arn: String,
    #[serde(rename = "UnsubscribeURL")]
    unsubscribe_url: String,

    #[serde(deserialize_with = "deserialize_json_string")]
    message: T,
}

fn deserialize_json_string<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    T: serde::de::DeserializeOwned,
    D: serde::Deserializer<'de>,
{
    use serde::de::Deserialize;
    let s = String::deserialize(deserializer)?;
    serde_json::from_str(&s).map_err(<D::Error as serde::de::Error>::custom)
}

#[derive(serde::Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct Mail {
    destination: Vec<String>,
    message_id: String,
    source: String,
    timestamp: String,
    common_headers: Option<CommonMailHeaders>,
    #[serde(default)]
    headers: Vec<MailHeader>,
    #[serde(default)]
    headers_truncated: bool,
}

#[derive(serde::Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct S3Action {
    bucket_name: String,
    object_key: String,
    object_key_prefix: String,
    topic_arn: String,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(serde::Deserialize, Debug)]
struct Verdict {
    status: String,
}

#[derive(serde::Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct S3Receipt {
    action: S3Action,
    dkim_verdict: Verdict,
    processing_time_millis: i64,
    recipients: Vec<String>,
    spam_verdict: Verdict,
    spf_verdict: Verdict,
    timestamp: String,
    virus_verdict: Verdict,
}

#[derive(serde::Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct S3Email {
    mail: Mail,
    notification_type: String,
    receipt: S3Receipt,
}

#[derive(serde::Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct CommonMailHeaders {
    date: String,
    from: Vec<String>,
    message_id: String,
    return_path: String,
    subject: String,
    to: Vec<String>,
}
#[derive(serde::Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct MailHeader {
    name: String,
    value: String,
}
