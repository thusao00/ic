#![no_main]
use candid::Decode;
use ic_ic00_types::InstallCodeArgs;
use libfuzzer_sys::fuzz_target;

// This fuzz test feeds binary data to Candid's `Decode!` macro for InstallCodeArgs with the goal of exposing panics
// e.g. caused by stack overflows during decoding.

fuzz_target!(|data: &[u8]| {
    let payload = data.to_vec();
    let _decoded = match Decode!(payload.as_slice(), InstallCodeArgs) {
        Ok(_v) => _v,
        Err(_e) => return,
    };
});
