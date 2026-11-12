//! The native wire's answers never panic a host's link, and never hand it an object that is not
//! what its id names (native sync design §2.2, V1): the input, as everything a peer sends after
//! a request, either reads as an envelope line and chunks of blobs and changesets, each one's
//! bytes hashing to its id, or is refused. The link holds at most a line and one chunk.
#![no_main]

use libfuzzer_sys::fuzz_target;
use swarmops_core::{ObjectKind, hash_object};
use swarmops_ops::native::link::{Link, StreamLink};

/// The request every input answers: its bytes do not matter to the link.
const REQUEST: &[u8] = br#"{"op":"wire_get","api":1,"protocol":1,"req":{"under":"main"}}"#;

fuzz_target!(|data: &[u8]| {
    // 1. The input as the peer's answer.
    let mut link = StreamLink::new(std::io::Cursor::new(data.to_vec()), std::io::sink());
    let Ok(mut answer) = link.call(REQUEST, b"") else {
        return;
    };

    // 2. Every chunk it announces, each object checked against its id.
    while let Ok(Some(objects)) = answer.next_chunk() {
        for object in objects {
            assert!(matches!(
                object.kind(),
                ObjectKind::Blob | ObjectKind::Changeset
            ));
            assert_eq!(object.id(), hash_object(object.kind(), object.payload()));
        }
    }

    // 3. Nothing held beyond what came.
    let came = u64::try_from(data.len()).unwrap_or(u64::MAX);
    assert!(link.peak_bytes() <= came, "{} of {came}", link.peak_bytes());
});
