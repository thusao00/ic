use ic_interfaces::batch_payload::PastPayload;
use ic_logger::{error, ReplicaLogger};
use ic_protobuf::{
    canister_http::v1 as canister_http_pb,
    canister_http::v1::{canister_http_response_message::MessageType, CanisterHttpResponseMessage},
    proxy::ProxyDecodeError,
};
use ic_types::{batch::CanisterHttpPayload, messages::CallbackId, NumBytes};
use prost::{bytes::BufMut, DecodeError, Message};
use std::collections::HashSet;

pub(crate) fn bytes_to_payload(data: &[u8]) -> Result<CanisterHttpPayload, ProxyDecodeError> {
    let messages: Vec<CanisterHttpResponseMessage> =
        slice_to_messages(data).map_err(ProxyDecodeError::DecodeError)?;
    let mut payload = CanisterHttpPayload::default();

    for message in messages {
        match message.message_type {
            Some(MessageType::Timeout(timeout)) => payload.timeouts.push(CallbackId::new(timeout)),
            Some(MessageType::Response(response)) => payload.responses.push(response.try_into()?),
            Some(MessageType::DivergenceResponse(response)) => {
                payload.divergence_responses.push(response.try_into()?)
            }
            None => return Err(ProxyDecodeError::MissingField("message_type")),
        }
    }

    Ok(payload)
}

pub(crate) fn payload_to_bytes(payload: &CanisterHttpPayload, max_size: NumBytes) -> Vec<u8> {
    let message_iterator =
        payload
            .timeouts
            .iter()
            .map(|timeout| CanisterHttpResponseMessage {
                message_type: Some(MessageType::Timeout(timeout.get())),
            })
            .chain(payload.divergence_responses.iter().map(|response| {
                CanisterHttpResponseMessage {
                    message_type: Some(MessageType::DivergenceResponse(
                        canister_http_pb::CanisterHttpResponseDivergence::from(response),
                    )),
                }
            }))
            .chain(
                payload
                    .responses
                    .iter()
                    .map(|response| CanisterHttpResponseMessage {
                        message_type: Some(MessageType::Response(
                            canister_http_pb::CanisterHttpResponseWithConsensus::from(response),
                        )),
                    }),
            );

    iterator_to_vec(message_iterator, max_size)
}

pub(crate) fn parse_past_payload_ids(
    past_payloads: &[PastPayload],
    log: &ReplicaLogger,
) -> HashSet<CallbackId> {
    past_payloads
        .iter()
        .flat_map(|payload| {
            slice_to_messages::<CanisterHttpResponseMessage>(payload.payload).unwrap_or_else(
                |err| {
                    error!(
                        log,
                        "Failed to parse CanisterHttp past payload for height {}. Error: {}",
                        payload.height,
                        err
                    );
                    vec![]
                },
            )
        })
        .filter_map(get_id_from_message)
        .map(CallbackId::new)
        .collect()
}

/// Extracts the CanisterId (as u64) from a [`CanisterHttpResponseMessage`]
fn get_id_from_message(message: CanisterHttpResponseMessage) -> Option<u64> {
    match message.message_type {
        Some(MessageType::Response(response)) => response.response.map(|response| response.id),
        // NOTE: We simply use the id from the first metadata share
        // All metadata shares have the same id, otherwise they would not have been included as a past payload
        Some(MessageType::DivergenceResponse(response)) => response
            .shares
            .get(0)
            .and_then(|share| share.metadata.as_ref().map(|md| md.id)),
        Some(MessageType::Timeout(id)) => Some(id),
        None => None,
    }
}

fn iterator_to_vec<I, M>(iter: I, max_size: NumBytes) -> Vec<u8>
where
    M: Message,
    I: Iterator<Item = M>,
{
    let mut buffer = vec![].limit(max_size.get() as usize);

    for val in iter {
        // NOTE: This call may fail due to the encoding hitting the
        // byte limit. We continue trying the rest of the messages
        // nonetheless, to give smaller messages a chance as well
        let _ = val.encode_length_delimited(&mut buffer);
    }

    buffer.into_inner()
}

fn slice_to_messages<M>(mut data: &[u8]) -> Result<Vec<M>, DecodeError>
where
    M: Message + Default,
{
    let mut msgs = vec![];

    while !data.is_empty() {
        let msg = M::decode_length_delimited(&mut data)?;
        msgs.push(msg)
    }

    Ok(msgs)
}
