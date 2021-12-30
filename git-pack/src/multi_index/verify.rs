use std::cmp::Ordering;
use std::sync::atomic::AtomicBool;

use git_features::progress::Progress;

use crate::multi_index::File;

///
pub mod integrity {
    /// Returned by [`multi_index::File::verify_integrity()`][crate::multi_index::File::verify_integrity()].
    #[derive(thiserror::Error, Debug)]
    #[allow(missing_docs)]
    pub enum Error {
        #[error("Object {id} should be at pack-offset {expected_pack_offset} but was found at {actual_pack_offset}")]
        PackOffsetMismatch {
            id: git_hash::ObjectId,
            expected_pack_offset: u64,
            actual_pack_offset: u64,
        },
        #[error(transparent)]
        MultiIndexChecksum(#[from] crate::multi_index::verify::checksum::Error),
        #[error(transparent)]
        IndexIntegrity(#[from] crate::index::verify::integrity::Error),
        #[error(transparent)]
        BundleInit(#[from] crate::bundle::init::Error),
        #[error("Counted {actual} objects, but expected {expected} as per multi-index")]
        UnexpectedObjectCount { actual: usize, expected: usize },
        #[error("{id} wasn't found in the index referenced in the multi-pack index")]
        OidNotFound { id: git_hash::ObjectId },
        #[error("The object id at multi-index entry {index} wasn't in order")]
        OutOfOrder { index: u32 },
        #[error("The fan at index {index} is out of order as it's larger then the following value.")]
        Fan { index: usize },
        #[error("The multi-index claims to have no objects")]
        Empty,
    }

    /// Returned by [`multi_index::File::verify_integrity()`][crate::multi_index::File::verify_integrity()].
    pub struct Outcome<P> {
        /// The computed checksum of the multi-index which matched the stored one.
        pub actual_index_checksum: git_hash::ObjectId,
        /// The for each entry in [`index_names()`][super::File::index_names()] provide the corresponding pack traversal outcome.
        pub pack_traverse_outcomes: Vec<crate::index::traverse::Outcome>,
        /// The provided progress instance.
        pub progress: P,
    }
}

///
pub mod checksum {
    /// Returned by [`multi_index::File::verify_checksum()`][crate::multi_index::File::verify_checksum()].
    pub type Error = crate::verify::checksum::Error;
}

impl File {
    /// Validate that our [`checksum()`][File::checksum()] matches the actual contents
    /// of this index file, and return it if it does.
    pub fn verify_checksum(
        &self,
        progress: impl Progress,
        should_interrupt: &AtomicBool,
    ) -> Result<git_hash::ObjectId, checksum::Error> {
        crate::verify::checksum_on_disk_or_mmap(
            self.path(),
            &self.data,
            self.checksum(),
            self.object_hash,
            progress,
            should_interrupt,
        )
    }

    /// Similar to [`crate::Bundle::verify_integrity()`] but checks all contained indices and their packs.
    ///
    /// Note that it's considered a failure if an index doesn't have a corresponding pack.
    #[allow(unused)]
    pub fn verify_integrity<C, P>(
        &self,
        verify_mode: crate::index::verify::Mode,
        traversal: crate::index::traverse::Algorithm,
        make_pack_lookup_cache: impl Fn() -> C + Send + Clone,
        thread_limit: Option<usize>,
        mut progress: P,
        should_interrupt: &AtomicBool,
    ) -> Result<integrity::Outcome<P>, crate::index::traverse::Error<integrity::Error>>
    where
        P: Progress,
        C: crate::cache::DecodeEntry,
    {
        let parent = self.path.parent().expect("must be in a directory");

        let actual_index_checksum = self
            .verify_checksum(
                progress.add_child(format!("checksum of '{}'", self.path.display())),
                should_interrupt,
            )
            .map_err(integrity::Error::from)
            .map_err(crate::index::traverse::Error::Processor)?;

        if let Some(first_invalid) = crate::verify::fan(&self.fan) {
            return Err(crate::index::traverse::Error::Processor(integrity::Error::Fan {
                index: first_invalid,
            }));
        }

        if self.num_objects == 0 {
            return Err(crate::index::traverse::Error::Processor(integrity::Error::Empty));
        }

        let mut pack_traverse_outcomes = Vec::new();

        progress.set_name("Validating");
        let start = std::time::Instant::now();

        progress.init(
            Some(self.num_indices as usize),
            git_features::progress::count("indices"),
        );

        let order_start = std::time::Instant::now();
        let mut our_progress = progress.add_child("checking oid order");
        our_progress.init(
            Some(self.num_objects as usize),
            git_features::progress::count("objects"),
        );

        let mut pack_ids_and_offsets = Vec::with_capacity(self.num_objects as usize);
        let mut total_objects_checked = 0;
        for entry_index in 0..(self.num_objects - 1) {
            let lhs = self.oid_at_index(entry_index);
            let rhs = self.oid_at_index(entry_index + 1);

            if rhs.cmp(lhs) != Ordering::Greater {
                return Err(crate::index::traverse::Error::Processor(integrity::Error::OutOfOrder {
                    index: entry_index,
                }));
            }
            let (pack_id, _) = self.pack_id_and_pack_offset_at_index(entry_index);
            pack_ids_and_offsets.push((pack_id, entry_index));
            our_progress.inc();
        }
        {
            let entry_index = self.num_objects - 1;
            let (pack_id, _) = self.pack_id_and_pack_offset_at_index(entry_index);
            pack_ids_and_offsets.push((pack_id, entry_index));
        }
        // sort by pack-id to allow handling all indices matching a pack while its open.
        pack_ids_and_offsets.sort_by(|l, r| l.0.cmp(&r.0));
        our_progress.show_throughput(order_start);

        our_progress.set_name("verify object offsets");
        our_progress.set(0);

        let mut pack_ids_slice = pack_ids_and_offsets.as_slice();
        for (pack_id, index_file_name) in self.index_names.iter().enumerate() {
            progress.inc();
            let bundle = crate::Bundle::at(parent.join(index_file_name), self.object_hash)
                .map_err(integrity::Error::from)
                .map_err(crate::index::traverse::Error::Processor)?;

            let slice_end = pack_ids_slice.partition_point(|e| e.0 == pack_id as u32);
            let multi_index_entries_to_check = &pack_ids_slice[..slice_end];
            pack_ids_slice = &pack_ids_slice[slice_end..];

            for entry_id in multi_index_entries_to_check.iter().map(|e| e.1) {
                let oid = self.oid_at_index(entry_id);
                let (_, expected_pack_offset) = self.pack_id_and_pack_offset_at_index(entry_id);
                let entry_in_bundle_index = bundle.index.lookup(oid).ok_or_else(|| {
                    crate::index::traverse::Error::Processor(integrity::Error::OidNotFound { id: oid.to_owned() })
                })?;
                let actual_pack_offset = bundle.index.pack_offset_at_index(entry_in_bundle_index);
                if actual_pack_offset != expected_pack_offset {
                    return Err(crate::index::traverse::Error::Processor(
                        integrity::Error::PackOffsetMismatch {
                            id: oid.to_owned(),
                            expected_pack_offset,
                            actual_pack_offset,
                        },
                    ));
                }
                our_progress.inc();
            }

            total_objects_checked += multi_index_entries_to_check.len();

            let progress = progress.add_child(index_file_name.display().to_string());
            let crate::bundle::verify::integrity::Outcome {
                actual_index_checksum: _,
                pack_traverse_outcome,
                progress: _,
            } = bundle
                .verify_integrity(
                    verify_mode,
                    traversal,
                    make_pack_lookup_cache.clone(),
                    thread_limit,
                    progress,
                    should_interrupt,
                )
                .map_err(|err| {
                    use crate::index::traverse::Error::*;
                    match err {
                        Processor(err) => Processor(integrity::Error::IndexIntegrity(err)),
                        VerifyChecksum(err) => VerifyChecksum(err),
                        Tree(err) => Tree(err),
                        TreeTraversal(err) => TreeTraversal(err),
                        PackDecode { id, offset, source } => PackDecode { id, offset, source },
                        PackMismatch { expected, actual } => PackMismatch { expected, actual },
                        PackObjectMismatch {
                            expected,
                            actual,
                            offset,
                            kind,
                        } => PackObjectMismatch {
                            expected,
                            actual,
                            offset,
                            kind,
                        },
                        Crc32Mismatch {
                            expected,
                            actual,
                            offset,
                            kind,
                        } => Crc32Mismatch {
                            expected,
                            actual,
                            offset,
                            kind,
                        },
                        Interrupted => Interrupted,
                    }
                })?;
            pack_traverse_outcomes.push(pack_traverse_outcome);
        }

        assert_eq!(
            self.num_objects as usize, total_objects_checked,
            "BUG: our slicing should allow to visit all objects"
        );

        progress.show_throughput(start);

        Ok(integrity::Outcome {
            actual_index_checksum,
            pack_traverse_outcomes,
            progress,
        })
    }
}
