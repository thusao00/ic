use super::{
    checkpoint::{Checkpoint, MappingSerialization},
    page_allocator::PageAllocatorSerialization,
    Buffer, FileDescriptor, PageAllocator, PageAllocatorRegistry, PageDelta, PageIndex, PageMap,
    PageMapSerialization,
};
use crate::page_map::{MemoryRegion, TestPageAllocatorFileDescriptorImpl};
use ic_sys::PAGE_SIZE;
use ic_types::{Height, MAX_STABLE_MEMORY_IN_BYTES};
use nix::unistd::dup;
use std::sync::Arc;
use std::{fs::OpenOptions, ops::Range};

fn assert_equal_page_maps(page_map1: &PageMap, page_map2: &PageMap) {
    assert_eq!(page_map1.num_host_pages(), page_map2.num_host_pages());
    for i in 0..page_map1.num_host_pages() {
        assert_eq!(
            page_map1.get_page(PageIndex::new(i as u64)),
            page_map2.get_page(PageIndex::new(i as u64))
        );
    }
}

// Since tests run in the same process, we need to duplicate all file
// descriptors so that both page maps can close them.
fn duplicate_file_descriptors(
    mut serialized_page_map: PageMapSerialization,
) -> PageMapSerialization {
    serialized_page_map.checkpoint.mapping =
        serialized_page_map
            .checkpoint
            .mapping
            .map(|mapping| MappingSerialization {
                file_descriptor: FileDescriptor {
                    fd: dup(mapping.file_descriptor.fd).unwrap(),
                },
                ..mapping
            });
    serialized_page_map.page_allocator = PageAllocatorSerialization {
        id: serialized_page_map.page_allocator.id,
        fd: FileDescriptor {
            fd: dup(serialized_page_map.page_allocator.fd.fd).unwrap(),
        },
    };
    serialized_page_map
}

#[test]
fn can_debug_display_a_page_map() {
    let page_map = PageMap::new_for_testing();
    assert_eq!(format!("{:?}", page_map), "{}");
}

#[test]
fn can_create_an_empty_checkpoint() {
    let checkpoint = Checkpoint::empty();
    let empty_page = vec![0; PAGE_SIZE];
    let first_page = checkpoint.get_page(PageIndex::new(1));
    assert_eq!(&empty_page[..], first_page);
}

#[test]
fn empty_page_map_returns_zeroed_pages() {
    let page_map = PageMap::new_for_testing();
    let page = page_map.get_page(PageIndex::new(1));
    assert_eq!(page.len(), PAGE_SIZE);
    assert!(page.iter().all(|b| *b == 0));
}

#[test]
fn can_update_a_page_map() {
    let mut page_map = PageMap::new_for_testing();
    let ones = [1u8; PAGE_SIZE];
    let twos = [2u8; PAGE_SIZE];

    let delta = [(PageIndex::new(1), &ones), (PageIndex::new(2), &twos)];

    page_map.update(&delta);

    for (num, contents) in &[(1, 1), (2, 2), (3, 0)] {
        assert!(page_map
            .get_page(PageIndex::new(*num))
            .iter()
            .all(|b| *b == *contents));
    }
}

#[test]
fn new_delta_wins_on_update() {
    let mut page_map = PageMap::new_for_testing();
    let page_1 = [1u8; PAGE_SIZE];
    let page_2 = [2u8; PAGE_SIZE];

    let pages_1 = &[(PageIndex::new(1), &page_1)];
    let pages_2 = &[(PageIndex::new(1), &page_2)];

    page_map.update(pages_1);
    page_map.update(pages_2);

    assert_eq!(page_map.get_page(PageIndex::new(1)), &page_2);
}

#[test]
fn persisted_map_is_equivalent_to_the_original() {
    let tmp = tempfile::Builder::new()
        .prefix("checkpoints")
        .tempdir()
        .unwrap();
    let heap_file = tmp.path().join("heap");

    let base_page = [42u8; PAGE_SIZE];
    let base_data = vec![&base_page; 50];

    let base_pages: Vec<(PageIndex, &[u8; PAGE_SIZE])> = base_data
        .iter()
        .enumerate()
        .map(|(i, page)| (PageIndex::new(i as u64), *page))
        .collect();

    let mut base_map = PageMap::new_for_testing();
    base_map.update(base_pages.as_slice());
    base_map.persist_delta(&heap_file).unwrap();

    let mut original_map = PageMap::open(
        &heap_file,
        Height::new(0),
        Arc::new(TestPageAllocatorFileDescriptorImpl::new()),
    )
    .unwrap();

    assert_eq!(base_map, original_map);

    let page_1 = [1u8; PAGE_SIZE];
    let page_3 = [3u8; PAGE_SIZE];
    let page_4 = [4u8; PAGE_SIZE];
    let page_60 = [60u8; PAGE_SIZE];
    let page_62 = [62u8; PAGE_SIZE];
    let page_100 = [100u8; PAGE_SIZE];

    let pages = &[
        (PageIndex::new(1), &page_1),
        (PageIndex::new(3), &page_3),
        (PageIndex::new(4), &page_4),
        (PageIndex::new(60), &page_60),
        (PageIndex::new(62), &page_62),
        (PageIndex::new(100), &page_100),
    ];

    original_map.update(pages);

    original_map.persist_delta(&heap_file).unwrap();
    let persisted_map = PageMap::open(
        &heap_file,
        Height::new(0),
        Arc::new(TestPageAllocatorFileDescriptorImpl::new()),
    )
    .unwrap();

    assert_eq!(persisted_map, original_map);
}

#[test]
fn can_persist_and_load_an_empty_page_map() {
    let tmp = tempfile::Builder::new()
        .prefix("checkpoints")
        .tempdir()
        .unwrap();
    let heap_file = tmp.path().join("heap");

    let original_map = PageMap::new_for_testing();
    original_map.persist_delta(&heap_file).unwrap();
    let persisted_map = PageMap::open(
        &heap_file,
        Height::new(0),
        Arc::new(TestPageAllocatorFileDescriptorImpl::new()),
    )
    .expect("opening an empty page map must succeed");

    // base_height will be different, but is not part of eq
    assert_eq!(original_map, persisted_map);
}

#[test]
fn returns_an_error_if_file_size_is_not_a_multiple_of_page_size() {
    use std::io::Write;

    let tmp = tempfile::Builder::new()
        .prefix("checkpoints")
        .tempdir()
        .unwrap();
    let heap_file = tmp.path().join("heap");
    OpenOptions::new()
        .write(true)
        .create(true)
        .open(&heap_file)
        .unwrap()
        .write_all(&vec![1; PAGE_SIZE / 2])
        .unwrap();

    match PageMap::open(
        &heap_file,
        Height::new(0),
        Arc::new(TestPageAllocatorFileDescriptorImpl::new()),
    ) {
        Err(err) => assert!(
            err.is_invalid_heap_file(),
            "Expected invalid heap file error, got {:?}",
            err
        ),
        Ok(_) => panic!("Expected a invalid heap file error, got Ok(_)"),
    }
}

#[test]
fn can_use_buffer_to_modify_page_map() {
    let page_1 = [1u8; PAGE_SIZE];
    let page_3 = [3u8; PAGE_SIZE];
    let pages = &[(PageIndex::new(1), &page_1), (PageIndex::new(3), &page_3)];
    let mut page_map = PageMap::new_for_testing();
    page_map.update(pages);

    let n = 4 * PAGE_SIZE;
    let mut vec_buf = vec![0u8; n];
    vec_buf[PAGE_SIZE..2 * PAGE_SIZE].copy_from_slice(&page_1);
    vec_buf[3 * PAGE_SIZE..4 * PAGE_SIZE].copy_from_slice(&page_3);

    let mut buf = Buffer::new(page_map);

    let mut read_buf = vec![0u8; n];

    buf.read(&mut read_buf[..], 0);
    assert_eq!(read_buf, vec_buf);

    for offset in 0..n {
        let mut len = 1;
        while (offset + len) < n {
            let b = ((offset + len) % 15) as u8;
            for dst in vec_buf.iter_mut().skip(offset).take(len) {
                *dst = b;
            }
            buf.write(&vec_buf[offset..offset + len], offset);
            buf.read(&mut read_buf[..], 0);
            assert_eq!(read_buf, vec_buf);
            len *= 2;
        }
    }
}

#[test]
fn serialize_empty_page_map() {
    let page_allocator_registry = PageAllocatorRegistry::new();
    let original_page_map = PageMap::new_for_testing();
    let serialized_page_map = duplicate_file_descriptors(original_page_map.serialize());
    let deserialized_page_map =
        PageMap::deserialize(serialized_page_map, &page_allocator_registry).unwrap();
    assert_equal_page_maps(&original_page_map, &deserialized_page_map);
}

#[test]
fn serialize_page_map() {
    let page_allocator_registry = PageAllocatorRegistry::new();
    let mut replica = PageMap::new_for_testing();
    // The replica process sends the page map to the sandbox process.
    let serialized_page_map = duplicate_file_descriptors(replica.serialize());
    let mut sandbox = PageMap::deserialize(serialized_page_map, &page_allocator_registry).unwrap();
    // The sandbox process allocates new pages.
    let page_1 = [1u8; PAGE_SIZE];
    let page_3 = [3u8; PAGE_SIZE];
    let page_7 = [7u8; PAGE_SIZE];
    let pages = &[(PageIndex::new(1), &page_1), (PageIndex::new(3), &page_3)];
    sandbox.update(pages);
    sandbox.strip_unflushed_delta();
    sandbox.update(&[(PageIndex::new(7), &page_7)]);
    // The sandbox process sends the dirty pages to the replica process.
    let page_delta =
        sandbox.serialize_delta(&[PageIndex::new(1), PageIndex::new(3), PageIndex::new(7)]);
    replica.deserialize_delta(page_delta);
    // The page deltas must be in sync.
    assert_equal_page_maps(&replica, &sandbox);
}

#[test]
fn write_amplification_is_calculated_correctly() {
    let allocator: PageAllocator = PageAllocator::new_for_testing();

    let page = [1u8; PAGE_SIZE];

    let pages = &[
        (PageIndex::new(1), &page),
        // gap 1
        (PageIndex::new(3), &page),
        (PageIndex::new(4), &page),
        // gap 100
        (PageIndex::new(105), &page),
    ];

    let pages = allocator.allocate(pages);

    let delta = PageDelta::from(pages);

    // Amplification of 1 doesn't allow gaps
    assert_eq!(delta.write_amplification_to_gap(1000, 1.0), 0);

    // Amplification smaller than 1 is safe
    assert_eq!(delta.write_amplification_to_gap(1000, 0.5), 0);
    assert_eq!(delta.write_amplification_to_gap(1000, -10.0), 0);

    // Small amplification should allow the small gap, but not the large
    assert!(delta.write_amplification_to_gap(1000, 2.0) < 100);
    assert!(delta.write_amplification_to_gap(1000, 2.0) >= 1);

    // Large amplification should allow both gaps
    assert!(delta.write_amplification_to_gap(1000, 100.0) >= 100);

    // Maximum gap is respected
    assert_eq!(delta.write_amplification_to_gap(50, 100.0), 50);
}

/// Check that the value provided by `calculate_dirty_pages` agrees with the
/// actual change in number of dirty pages and return the number of new dirty
/// pages.
fn write_and_verify_dirty_pages(buf: &mut Buffer, src: &[u8], offset: usize) -> u64 {
    let new = buf.dirty_pages_from_write(offset as u64, src.len() as u64);
    let initial = buf.dirty_pages.len();
    buf.write(src, offset);
    let updated = buf.dirty_pages.len();
    assert_eq!(updated - initial, new.get() as usize);
    new.get()
}

/// Complete re-write of first page is dirty, later write doesn't increase
/// count.
#[test]
fn buffer_entire_first_page_write() {
    let mut buf = Buffer::new(PageMap::new_for_testing());
    assert_eq!(
        1,
        write_and_verify_dirty_pages(&mut buf, &[0; PAGE_SIZE], 0)
    );
    assert_eq!(0, write_and_verify_dirty_pages(&mut buf, &[0; 1], 0));
}

/// Single write to first page is dirty, later write doesn't increase count.
#[test]
fn buffer_single_byte_first_page_write() {
    let mut buf = Buffer::new(PageMap::new_for_testing());
    assert_eq!(1, write_and_verify_dirty_pages(&mut buf, &[0; 1], 0));
    assert_eq!(0, write_and_verify_dirty_pages(&mut buf, &[0; 1], 1));
}

#[test]
fn buffer_write_single_byte_each_page() {
    let mut buf = Buffer::new(PageMap::new_for_testing());
    assert_eq!(1, write_and_verify_dirty_pages(&mut buf, &[0; 1], 0));
    assert_eq!(
        1,
        write_and_verify_dirty_pages(&mut buf, &[0; 1], PAGE_SIZE)
    );
    assert_eq!(
        1,
        write_and_verify_dirty_pages(&mut buf, &[0; 1], 2 * PAGE_SIZE)
    );
    assert_eq!(
        1,
        write_and_verify_dirty_pages(&mut buf, &[0; 1], 15 * PAGE_SIZE)
    );
}

#[test]
fn buffer_write_unaligned_multiple_pages() {
    const NUM_PAGES: u64 = 3;
    let mut buf = Buffer::new(PageMap::new_for_testing());
    assert_eq!(
        NUM_PAGES + 1,
        write_and_verify_dirty_pages(&mut buf, &[0; (NUM_PAGES as usize) * PAGE_SIZE], 24)
    );
}

#[test]
fn buffer_write_empty_slice() {
    let mut buf = Buffer::new(PageMap::new_for_testing());
    assert_eq!(0, write_and_verify_dirty_pages(&mut buf, &[0; 0], 10_000));
}

// Checks that the pre-computed dirty pages agrees with the difference in dirty
// pages from before and after a write.
#[test]
fn calc_dirty_pages_matches_actual_change() {
    let mut runner = proptest::test_runner::TestRunner::deterministic();
    runner
        .run(
            &(0..MAX_STABLE_MEMORY_IN_BYTES, 0..(1000 * PAGE_SIZE as u64)),
            |(offset, size)| {
                // bound size to valid range
                let size = (MAX_STABLE_MEMORY_IN_BYTES - offset).min(size);
                let src = vec![0; size as usize];
                // Start with a buffer that has some initial dirty pages
                let mut buffer = Buffer::new(PageMap::new_for_testing());
                buffer.write(&[1; 10 * PAGE_SIZE], 5 * PAGE_SIZE + 10);
                buffer.write(&[3; 16], 44 * PAGE_SIZE);

                write_and_verify_dirty_pages(&mut buffer, &src, offset as usize);
                Ok(())
            },
        )
        .unwrap()
}

#[test]
fn zeros_region_after_delta() {
    let mut page_map = PageMap::new_for_testing();
    let tmp = tempfile::Builder::new()
        .prefix("checkpoints")
        .tempdir()
        .unwrap();
    let heap_file = tmp.path().join("heap");
    let pages = &[(PageIndex::new(1), &[1u8; PAGE_SIZE])];
    page_map.update(pages);

    page_map.persist_delta(&heap_file).unwrap();

    let mut page_map = PageMap::open(
        &heap_file,
        Height::new(0),
        Arc::new(TestPageAllocatorFileDescriptorImpl::new()),
    )
    .unwrap();

    let zero_range = page_map.get_memory_region(PageIndex::new(5));
    assert_eq!(
        MemoryRegion::Zeros(Range {
            start: PageIndex::new(2),
            end: PageIndex::new(u64::MAX)
        }),
        zero_range
    );

    let pages = &[(PageIndex::new(3), &[1u8; PAGE_SIZE])];
    page_map.update(pages);

    let zero_range = page_map.get_memory_region(PageIndex::new(5));
    assert_eq!(
        MemoryRegion::Zeros(Range {
            start: PageIndex::new(4),
            end: PageIndex::new(u64::MAX)
        }),
        zero_range
    );
}

#[test]
fn zeros_region_within_delta() {
    let mut page_map = PageMap::new_for_testing();
    let tmp = tempfile::Builder::new()
        .prefix("checkpoints")
        .tempdir()
        .unwrap();
    let heap_file = tmp.path().join("heap");
    let pages = &[(PageIndex::new(1), &[1u8; PAGE_SIZE])];
    page_map.update(pages);

    page_map.persist_delta(&heap_file).unwrap();

    let mut page_map = PageMap::open(
        &heap_file,
        Height::new(0),
        Arc::new(TestPageAllocatorFileDescriptorImpl::new()),
    )
    .unwrap();

    let zero_range = page_map.get_memory_region(PageIndex::new(5));
    assert_eq!(
        MemoryRegion::Zeros(Range {
            start: PageIndex::new(2),
            end: PageIndex::new(u64::MAX)
        }),
        zero_range
    );

    let pages = &[
        (PageIndex::new(3), &[1u8; PAGE_SIZE]),
        (PageIndex::new(10), &[1u8; PAGE_SIZE]),
    ];
    page_map.update(pages);

    let zero_range = page_map.get_memory_region(PageIndex::new(5));
    assert_eq!(
        MemoryRegion::Zeros(Range {
            start: PageIndex::new(4),
            end: PageIndex::new(10)
        }),
        zero_range
    );
}
