use ic_canonical_state::{Control, Visitor};
use ic_crypto_tree_hash::{HashTree, HashTreeBuilder, HashTreeBuilderImpl, Label};
use ic_replicated_state::ReplicatedState;

/// A visitor that constructs a hash tree by traversing a replicated
/// state.
#[derive(Default)]
pub struct HashingVisitor<T> {
    tree_hasher: T,
}

impl<T> Visitor for HashingVisitor<T>
where
    T: HashTreeBuilder,
{
    type Output = T;

    fn start_subtree(&mut self) -> Result<(), Self::Output> {
        self.tree_hasher.start_subtree();
        Ok(())
    }

    fn enter_edge(&mut self, label: &[u8]) -> Result<Control, Self::Output> {
        self.tree_hasher.new_edge(Label::from(label));
        Ok(Control::Continue)
    }

    fn end_subtree(&mut self) -> Result<(), Self::Output> {
        self.tree_hasher.finish_subtree();
        Ok(())
    }

    fn visit_num(&mut self, num: u64) -> Result<(), Self::Output> {
        self.tree_hasher.start_leaf();
        self.tree_hasher.write_leaf(&num.to_le_bytes()[..]);
        self.tree_hasher.finish_leaf();
        Ok(())
    }

    fn visit_blob(&mut self, blob: &[u8]) -> Result<(), Self::Output> {
        self.tree_hasher.start_leaf();
        self.tree_hasher.write_leaf(blob);
        self.tree_hasher.finish_leaf();
        Ok(())
    }

    fn finish(self) -> Self::Output {
        self.tree_hasher
    }
}

/// Compute the hash tree corresponding to the full replicated state.
pub fn hash_state(state: &ReplicatedState) -> HashTree {
    ic_canonical_state::traverse(state, HashingVisitor::<HashTreeBuilderImpl>::default())
        .into_hash_tree()
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hex::FromHex;
    use ic_base_types::{NumBytes, NumSeconds};
    use ic_canonical_state::CertificationVersion;
    use ic_crypto_tree_hash::Digest;
    use ic_error_types::{ErrorCode, UserError};
    use ic_registry_routing_table::{CanisterIdRange, RoutingTable};
    use ic_registry_subnet_type::SubnetType;
    use ic_replicated_state::{
        canister_state::execution_state::{
            CustomSection, CustomSectionType, NextScheduledMethod, WasmBinary, WasmMetadata,
        },
        metadata_state::Stream,
        page_map::{PageIndex, PAGE_SIZE},
        testing::ReplicatedStateTesting,
        ExecutionState, ExportedFunctions, Global, Memory, NumWasmPages, PageMap, ReplicatedState,
    };
    use ic_test_utilities::{
        state::new_canister_state,
        types::ids::{canister_test_id, message_test_id, subnet_test_id, user_test_id},
        types::messages::ResponseBuilder,
    };
    use ic_types::{
        crypto::CryptoHash,
        ingress::{IngressState, IngressStatus},
        xnet::{StreamIndex, StreamIndexedQueue},
        CryptoHashOfPartialState, Cycles, ExecutionRound, Time,
    };
    use ic_wasm_types::CanisterModule;
    use maplit::btreemap;
    use std::{collections::BTreeSet, sync::Arc};
    use strum::{EnumCount, IntoEnumIterator};

    const INITIAL_CYCLES: Cycles = Cycles::new(1 << 36);

    #[test]
    fn partial_hash_reflects_streams() {
        let mut state = ReplicatedState::new(subnet_test_id(1), SubnetType::Application);

        let hash_of_empty_state = hash_state(&state);

        state.modify_streams(|streams| {
            streams.insert(
                subnet_test_id(5),
                Stream::new(
                    StreamIndexedQueue::with_begin(StreamIndex::new(4)),
                    StreamIndex::new(10),
                ),
            );
        });

        let hash_of_state_with_streams = hash_state(&state);

        assert!(
            hash_of_empty_state != hash_of_state_with_streams,
            "Expected the hash tree of the empty state {:?} to different from the hash tree with streams {:?}",
            hash_of_empty_state, hash_of_state_with_streams
        );
    }

    #[test]
    fn partial_hash_detects_changes_in_streams() {
        use ic_replicated_state::metadata_state::Stream;
        use ic_types::xnet::{StreamIndex, StreamIndexedQueue};

        let mut state = ReplicatedState::new(subnet_test_id(1), SubnetType::Application);

        let stream = Stream::new(
            StreamIndexedQueue::with_begin(StreamIndex::from(4)),
            StreamIndex::new(10),
        );

        state.modify_streams(|streams| {
            streams.insert(subnet_test_id(5), stream);
        });

        let hash_of_state_one = hash_state(&state);

        let stream = Stream::new(
            StreamIndexedQueue::with_begin(StreamIndex::from(14)),
            StreamIndex::new(11),
        );
        state.modify_streams(|streams| {
            streams.insert(subnet_test_id(6), stream);
        });

        let hash_of_state_two = hash_state(&state);

        assert!(
            hash_of_state_one != hash_of_state_two,
            "Expected the hash tree of one stream {:?} to different from the hash tree with two streams {:?}",
            hash_of_state_one, hash_of_state_two
        );
    }

    #[test]
    fn test_backward_compatibility() {
        fn state_fixture(certification_version: CertificationVersion) -> ReplicatedState {
            let subnet_id = subnet_test_id(1);
            let mut state = ReplicatedState::new(subnet_id, SubnetType::Application);

            let canister_id = canister_test_id(2);
            let controller = user_test_id(24);
            let mut canister_state = new_canister_state(
                canister_id,
                controller.get(),
                INITIAL_CYCLES,
                NumSeconds::from(100_000),
            );
            let mut wasm_memory = Memory::new(PageMap::new_for_testing(), NumWasmPages::from(2));
            wasm_memory
                .page_map
                .update(&[(PageIndex::from(1), &[0u8; PAGE_SIZE])]);
            let wasm_binary = WasmBinary::new(CanisterModule::new(vec![]));
            let metadata = WasmMetadata::new(btreemap! {
                String::from("dummy1") => CustomSection::new(CustomSectionType::Private, vec![0, 2]),
            });
            let execution_state = ExecutionState {
                canister_root: "NOT_USED".into(),
                session_nonce: None,
                wasm_binary,
                wasm_memory,
                stable_memory: Memory::new_for_testing(),
                exported_globals: vec![Global::I32(1)],
                exports: ExportedFunctions::new(BTreeSet::new()),
                metadata,
                last_executed_round: ExecutionRound::from(0),
                next_scheduled_method: NextScheduledMethod::default(),
            };
            canister_state.execution_state = Some(execution_state);

            state.put_canister_state(canister_state);

            let mut stream = Stream::new(
                StreamIndexedQueue::with_begin(StreamIndex::from(4)),
                StreamIndex::new(10),
            );
            for _ in 1..6 {
                stream.push(ResponseBuilder::new().build().into());
            }
            if certification_version >= CertificationVersion::V8 {
                stream.push_reject_signal(10.into());
                stream.increment_signals_end();
            }
            state.modify_streams(|streams| {
                streams.insert(subnet_test_id(5), stream);
            });

            for i in 1..6 {
                state.set_ingress_status(
                    message_test_id(i),
                    IngressStatus::Unknown,
                    NumBytes::from(u64::MAX),
                );
            }

            if certification_version >= CertificationVersion::V11 {
                state.set_ingress_status(
                    message_test_id(7),
                    IngressStatus::Known {
                        state: IngressState::Failed(UserError::new(
                            ErrorCode::CanisterNotFound,
                            "canister not found",
                        )),
                        receiver: canister_id.into(),
                        user_id: user_test_id(1),
                        time: Time::from_nanos_since_unix_epoch(12345),
                    },
                    NumBytes::from(u64::MAX),
                );
            }

            let mut routing_table = RoutingTable::new();
            routing_table
                .insert(
                    CanisterIdRange {
                        start: canister_id,
                        end: canister_id,
                    },
                    subnet_id,
                )
                .unwrap();
            state.metadata.network_topology.subnets = btreemap! {
                subnet_id => Default::default(),
            };
            state.metadata.network_topology.routing_table = Arc::new(routing_table);
            state.metadata.prev_state_hash =
                Some(CryptoHashOfPartialState::new(CryptoHash(vec![3, 2, 1])));

            state.metadata.certification_version = certification_version;

            state
        }

        fn assert_partial_state_hash_matches(
            certification_version: CertificationVersion,
            expected_hash: &str,
        ) {
            let state = state_fixture(certification_version);

            assert_eq!(
                hash_state(&state).digest(),
                &Digest::from(<[u8; 32]>::from_hex(expected_hash,).unwrap()),
                "Mismatched partial state hash computed according to certification version {:?}. \
                Perhaps you made a change that requires writing backward compatibility code?",
                certification_version
            );
        }

        // WARNING: IF THIS TEST FAILS IT IS LIKELY BECAUSE OF A CHANGE THAT BREAKS
        // BACKWARD COMPATIBILITY OF PARTIAL STATE HASHING. IF THAT IS THE CASE
        // PLEASE INCREMENT THE CERTIFICATION VERSION AND PROVIDE APPROPRIATE
        // BACKWARD COMPATIBILITY CODE FOR OLD CERTIFICATION VERSIONS THAT
        // NEED TO BE SUPPORTED.
        let expected_hashes: [&str; CertificationVersion::COUNT] = [
            "C6BC681D0760A9CF36232892FE14E045ECE4EC406BF46117334DDE0E3603A6D5",
            "598F69AB872954AF52188C640BF3C180E90821F259225B3CD5EFCD2AD9EF8F88",
            "B120396B7F0885B30E52D3BACDA38E9EB2C07C054E8E4045E845AF15B97844C4",
            "52029C1F4C483B2B69ADF77AC9877D2E7A305BD06B4D9A10E95B7B1AC9B0464C",
            "52029C1F4C483B2B69ADF77AC9877D2E7A305BD06B4D9A10E95B7B1AC9B0464C",
            "52029C1F4C483B2B69ADF77AC9877D2E7A305BD06B4D9A10E95B7B1AC9B0464C",
            "A08B206B6E2D2B0F2EE3D334C01AD79163BECDE24FAF21723F5D1F434357F5AA",
            "A08B206B6E2D2B0F2EE3D334C01AD79163BECDE24FAF21723F5D1F434357F5AA",
            "D963A967586652BBBAFBD630A1DB53442F01548A5AC42E5A33D1BFEF61BFD9A0",
            "D963A967586652BBBAFBD630A1DB53442F01548A5AC42E5A33D1BFEF61BFD9A0",
            "1213C1D177E064FB70CB9B62BFE20DB823A109B71B4DAC7E41AEAE07DEFDA6FC",
            "C3F332850C080533635500BE033EF6383321032644914CF3356EFC9733A3E55D",
        ];
        for certification_version in CertificationVersion::iter() {
            assert_partial_state_hash_matches(
                certification_version,
                // expected_hash
                expected_hashes[certification_version as usize],
            );
        }
    }
}
