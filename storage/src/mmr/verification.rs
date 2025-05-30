//! Defines the inclusion `Proof` structure, functions for generating them from any MMR implementing
//! the `Storage` trait, and functions for verifying them against a root hash.

use crate::mmr::{
    hasher::Hasher,
    iterator::{PathIterator, PeakIterator},
    Error,
    Error::*,
};
use bytes::{Buf, BufMut};
use commonware_codec::{FixedSize, ReadExt};
use commonware_cryptography::{Digest, Hasher as CHasher};
use futures::future::try_join_all;
use std::future::Future;
use tracing::debug;

/// Contains the information necessary for proving the inclusion of an element, or some range of elements, in the MMR
/// from its root hash.
///
/// The `hashes` vector contains:
///
/// 1: the hashes of each peak corresponding to a mountain containing no elements from the element range being proven
/// in decreasing order of height, followed by:
///
/// 2: the nodes in the remaining mountains necessary for reconstructing their peak hashes from the elements within
/// the range, ordered by the position of their parent.
#[derive(Clone, Debug, Eq)]
pub struct Proof<H: CHasher> {
    /// The total number of nodes in the MMR.
    pub size: u64,
    /// The hashes necessary for proving the inclusion of an element, or range of elements, in the
    /// MMR.
    pub hashes: Vec<H::Digest>,
}

/// A trait that allows generic generation of an MMR inclusion proof.
pub trait Storage<D: Digest> {
    /// Return the number of elements in the MMR.
    fn size(&self) -> u64;

    /// Return the specified node of the MMR if it exists & hasn't been pruned.
    fn get_node(&self, position: u64) -> impl Future<Output = Result<Option<D>, Error>> + Send;
}

impl<H: CHasher> PartialEq for Proof<H> {
    fn eq(&self, other: &Self) -> bool {
        self.size == other.size && self.hashes == other.hashes
    }
}

impl<H: CHasher> Proof<H> {
    /// Return true if `proof` proves that `element` appears at position `element_pos` within the MMR
    /// with root hash `root_hash`.
    pub fn verify_element_inclusion(
        &self,
        hasher: &mut H,
        element: &[u8],
        element_pos: u64,
        root_hash: &H::Digest,
    ) -> bool {
        self.verify_range_inclusion(hasher, &[element], element_pos, element_pos, root_hash)
    }

    /// Return true if `proof` proves that the `elements` appear consecutively between positions
    /// `start_element_pos` through `end_element_pos` (inclusive) within the MMR with root hash
    /// `root_hash`.
    pub fn verify_range_inclusion<T>(
        &self,
        hasher: &mut H,
        elements: T,
        start_element_pos: u64,
        end_element_pos: u64,
        root_hash: &H::Digest,
    ) -> bool
    where
        T: IntoIterator<Item: AsRef<[u8]>>,
    {
        let peak_hashes = match self.reconstruct_peak_hashes(
            hasher,
            elements,
            start_element_pos,
            end_element_pos,
        ) {
            Ok(peak_hashes) => peak_hashes,
            Err(e) => {
                debug!("failed to reconstruct peak hashes from proof: {:?}", e);
                return false;
            }
        };
        let mut mmr_hasher = Hasher::<H>::new(hasher);
        *root_hash == mmr_hasher.root_hash(self.size, peak_hashes.iter())
    }

    /// Reconstruct the peak hashes of the MMR that produced this proof, returning `MissingHashes`
    /// error if there are not enough proof hashes, or `ExtraHashes` error if not all proof hashes
    /// were used in the reconstruction.
    fn reconstruct_peak_hashes<T>(
        &self,
        hasher: &mut H,
        elements: T,
        start_element_pos: u64,
        end_element_pos: u64,
    ) -> Result<Vec<H::Digest>, Error>
    where
        T: IntoIterator<Item: AsRef<[u8]>>,
    {
        let mut proof_hashes_iter = self.hashes.iter();
        let mut siblings_iter = self.hashes.iter().rev();
        let mut mmr_hasher = Hasher::<H>::new(hasher);
        let mut elements_iter = elements.into_iter();

        // Include peak hashes only for trees that have no elements from the range, and keep track
        // of the starting and ending trees of those that do contain some.
        let mut peak_hashes: Vec<H::Digest> = Vec::new();
        let mut proof_hashes_used = 0;
        for (peak_pos, height) in PeakIterator::new(self.size) {
            let leftmost_pos = peak_pos + 2 - (1 << (height + 1));
            if peak_pos >= start_element_pos && leftmost_pos <= end_element_pos {
                match peak_hash_from_range(
                    &mut mmr_hasher,
                    peak_pos,
                    1 << height,
                    start_element_pos,
                    end_element_pos,
                    &mut elements_iter,
                    &mut siblings_iter,
                ) {
                    Ok(peak_hash) => peak_hashes.push(peak_hash),
                    Err(_) => return Err(MissingHashes),
                }
            } else if let Some(hash) = proof_hashes_iter.next() {
                proof_hashes_used += 1;
                peak_hashes.push(*hash);
            } else {
                return Err(MissingHashes);
            }
        }

        if elements_iter.next().is_some() {
            return Err(ExtraHashes);
        }
        let next_sibling = siblings_iter.next();
        if (proof_hashes_used == 0 && next_sibling.is_some())
            || (next_sibling.is_some()
                && *next_sibling.unwrap() != self.hashes[proof_hashes_used - 1])
        {
            return Err(ExtraHashes);
        }

        Ok(peak_hashes)
    }

    /// Return the maximum size in bytes of any serialized `Proof`.
    pub fn max_serialization_size() -> usize {
        u64::SIZE + (u8::MAX as usize * H::Digest::SIZE)
    }

    /// Canonically serializes the `Proof` as:
    /// ```text
    ///    [0-8): size (u64 big-endian)
    ///    [8-...): raw bytes of each hash, each of length `H::len()`
    /// ```
    pub fn serialize(&self) -> Vec<u8> {
        // A proof should never contain more hashes than the depth of the MMR, thus a single byte
        // for encoding the length of the hashes array still allows serializing MMRs up to 2^255
        // elements.
        assert!(
            self.hashes.len() <= u8::MAX as usize,
            "too many hashes in proof"
        );

        // Serialize the proof as a byte vector.
        let bytes_len = u64::SIZE + (self.hashes.len() * H::Digest::SIZE);
        let mut bytes = Vec::with_capacity(bytes_len);
        bytes.put_u64(self.size);
        for hash in self.hashes.iter() {
            bytes.extend_from_slice(hash.as_ref());
        }
        assert_eq!(bytes.len(), bytes_len, "serialization length mismatch");
        bytes.to_vec()
    }

    /// Deserializes a canonically encoded `Proof`. See `serialize` for the serialization format.
    pub fn deserialize(bytes: &[u8]) -> Option<Self> {
        let mut buf = bytes;
        if buf.len() < u64::SIZE {
            return None;
        }
        let size = buf.get_u64();

        // A proof should divide neatly into the hash length and not contain more than 255 hashes.
        let buf_remaining = buf.remaining();
        let hashes_len = buf_remaining / H::Digest::SIZE;
        if buf_remaining % H::Digest::SIZE != 0 || hashes_len > u8::MAX as usize {
            return None;
        }
        let mut hashes = Vec::with_capacity(hashes_len);
        for _ in 0..hashes_len {
            let digest = H::Digest::read(&mut buf).ok()?;
            hashes.push(digest);
        }
        Some(Self { size, hashes })
    }

    /// Return the list of pruned (pos < `start_pos`) node positions that are still required for
    /// proving any retained node.
    ///
    /// This set consists of every pruned node that is either (1) a peak, or (2) has no descendent
    /// in the retained section, but its immediate parent does. (A node meeting condition (2) can be
    /// shown to always be the left-child of its parent.)
    ///
    /// This set of nodes does not change with the MMR's size, only the pruning boundary. For a
    /// given pruning boundary that happens to be a valid MMR size, one can prove that this set is
    /// exactly the set of peaks for an MMR whose size equals the pruning boundary. If the pruning
    /// boundary is not a valid MMR size, then the set corresponds to the peaks of the largest MMR
    /// whose size is less than the pruning boundary.
    pub fn nodes_to_pin(start_pos: u64) -> impl Iterator<Item = u64> {
        PeakIterator::new(PeakIterator::to_nearest_size(start_pos)).map(|(pos, _)| pos)
    }

    /// Return the list of node positions required by the range proof for the specified range of
    /// elements, inclusive of both endpoints.
    pub fn nodes_required_for_range_proof(
        size: u64,
        start_element_pos: u64,
        end_element_pos: u64,
    ) -> Vec<u64> {
        let mut positions = Vec::<u64>::new();

        // Find the mountains that contain no elements from the range. The peaks of these mountains
        // are required to prove the range, so they are added to the result.
        let mut start_tree_with_element = (u64::MAX, 0);
        let mut end_tree_with_element = (u64::MAX, 0);
        let mut peak_iterator = PeakIterator::new(size);
        while let Some(item) = peak_iterator.next() {
            if start_tree_with_element.0 == u64::MAX && item.0 >= start_element_pos {
                // Found the first tree to contain an element in the range
                start_tree_with_element = item;
                if item.0 >= end_element_pos {
                    // Start and end tree are the same
                    end_tree_with_element = item;
                    continue;
                }
                for item in peak_iterator.by_ref() {
                    if item.0 >= end_element_pos {
                        // Found the last tree to contain an element in the range
                        end_tree_with_element = item;
                        break;
                    }
                }
            } else {
                // Tree is outside the range, its peak is thus required.
                positions.push(item.0);
            }
        }
        assert!(start_tree_with_element.0 != u64::MAX);
        assert!(end_tree_with_element.0 != u64::MAX);

        // Include the positions of any left-siblings of each node on the path from peak to
        // leftmost-leaf, and right-siblings for the path from peak to rightmost-leaf. These are
        // added in order of decreasing parent position.
        let left_path_iter = PathIterator::new(
            start_element_pos,
            start_tree_with_element.0,
            start_tree_with_element.1,
        );

        let mut siblings = Vec::<(u64, u64)>::new();
        if start_element_pos == end_element_pos {
            // For the (common) case of a single element range, the right and left path are the
            // same so no need to process each independently.
            siblings.extend(left_path_iter);
        } else {
            let right_path_iter = PathIterator::new(
                end_element_pos,
                end_tree_with_element.0,
                end_tree_with_element.1,
            );
            // filter the right path for right siblings only
            siblings.extend(right_path_iter.filter(|(parent_pos, pos)| *parent_pos == *pos + 1));
            // filter the left path for left siblings only
            siblings.extend(left_path_iter.filter(|(parent_pos, pos)| *parent_pos != *pos + 1));

            // If the range spans more than one tree, then the hashes must already be in the correct
            // order. Otherwise, we enforce the desired order through sorting.
            if start_tree_with_element.0 == end_tree_with_element.0 {
                siblings.sort_by(|a, b| b.0.cmp(&a.0));
            }
        }
        positions.extend(siblings.into_iter().map(|(_, pos)| pos));
        positions
    }

    /// Return an inclusion proof for the specified range of elements, inclusive of both endpoints.
    ///
    /// Returns ElementPruned error if some element needed to generate the proof has been pruned.
    pub async fn range_proof<S: Storage<H::Digest>>(
        mmr: &S,
        start_element_pos: u64,
        end_element_pos: u64,
    ) -> Result<Proof<H>, Error> {
        let mut hashes: Vec<H::Digest> = Vec::new();
        let positions =
            Self::nodes_required_for_range_proof(mmr.size(), start_element_pos, end_element_pos);

        let node_futures = positions.iter().map(|pos| mmr.get_node(*pos));
        let hash_results = try_join_all(node_futures).await?;

        for (i, hash_result) in hash_results.into_iter().enumerate() {
            match hash_result {
                Some(hash) => hashes.push(hash),
                None => return Err(Error::ElementPruned(positions[i])),
            };
        }

        Ok(Proof {
            size: mmr.size(),
            hashes,
        })
    }
}

fn peak_hash_from_range<'a, H, I, S>(
    hasher: &mut Hasher<H>,
    pos: u64,           // current node position in the tree
    two_h: u64,         // 2^height of the current node
    leftmost_pos: u64,  // leftmost leaf in the tree to be traversed
    rightmost_pos: u64, // rightmost leaf in the tree to be traversed
    elements: &mut I,
    sibling_hashes: &mut S,
) -> Result<H::Digest, ()>
where
    H: CHasher,
    I: Iterator<Item: AsRef<[u8]>>,
    S: Iterator<Item = &'a H::Digest>,
{
    assert_ne!(two_h, 0);
    if two_h == 1 {
        // we are at a leaf
        match elements.next() {
            Some(element) => return Ok(hasher.leaf_hash(pos, element.as_ref())),
            None => return Err(()),
        }
    }

    let left_pos = pos - two_h;
    let mut left_hash: Option<H::Digest> = None;
    let right_pos = left_pos + two_h - 1;
    let mut right_hash: Option<H::Digest> = None;

    if left_pos >= leftmost_pos {
        // Descend left
        match peak_hash_from_range(
            hasher,
            left_pos,
            two_h >> 1,
            leftmost_pos,
            rightmost_pos,
            elements,
            sibling_hashes,
        ) {
            Ok(h) => left_hash = Some(h),
            Err(_) => return Err(()),
        }
    }
    if left_pos < rightmost_pos {
        // Descend right
        match peak_hash_from_range(
            hasher,
            right_pos,
            two_h >> 1,
            leftmost_pos,
            rightmost_pos,
            elements,
            sibling_hashes,
        ) {
            Ok(h) => right_hash = Some(h),
            Err(_) => return Err(()),
        }
    }

    if left_hash.is_none() {
        match sibling_hashes.next() {
            Some(hash) => left_hash = Some(*hash),
            None => return Err(()),
        }
    }
    if right_hash.is_none() {
        match sibling_hashes.next() {
            Some(hash) => right_hash = Some(*hash),
            None => return Err(()),
        }
    }
    Ok(hasher.node_hash(pos, &left_hash.unwrap(), &right_hash.unwrap()))
}

#[cfg(test)]
mod tests {
    use super::Proof;
    use crate::mmr::mem::Mmr;
    use commonware_cryptography::{hash, sha256::Digest, Sha256};
    use commonware_runtime::{deterministic, Runner};

    fn test_digest(v: u8) -> Digest {
        hash(&[v])
    }

    #[test]
    fn test_verification_verify_element() {
        let executor = deterministic::Runner::default();
        executor.start(|_| async move {
            // create an 11 element MMR over which we'll test single-element inclusion proofs
            let mut mmr = Mmr::<Sha256>::new();
            let element = Digest::from(*b"01234567012345670123456701234567");
            let mut leaves: Vec<u64> = Vec::new();
            let mut hasher = Sha256::default();
            for _ in 0..11 {
                leaves.push(mmr.add(&mut hasher, &element));
            }

            let root_hash = mmr.root(&mut hasher);
            let mut hasher = Sha256::default();

            // confirm the proof of inclusion for each leaf successfully verifies
            for leaf in leaves.iter().by_ref() {
                let proof = mmr.proof(*leaf).await.unwrap();
                assert!(
                    proof.verify_element_inclusion(&mut hasher, &element, *leaf, &root_hash),
                    "valid proof should verify successfully"
                );
            }

            // confirm mangling the proof or proof args results in failed validation
            const POS: u64 = 18;
            let proof = mmr.proof(POS).await.unwrap();
            assert!(
                proof.verify_element_inclusion(&mut hasher, &element, POS, &root_hash),
                "proof verification should be successful"
            );
            assert!(
                !proof.verify_element_inclusion(&mut hasher, &element, POS + 1, &root_hash,),
                "proof verification should fail with incorrect element position"
            );
            assert!(
                !proof.verify_element_inclusion(&mut hasher, &element, POS - 1, &root_hash),
                "proof verification should fail with incorrect element position 2"
            );
            assert!(
                !proof.verify_element_inclusion(&mut hasher, &test_digest(0), POS, &root_hash,),
                "proof verification should fail with mangled element"
            );
            let root_hash2 = test_digest(0);
            assert!(
                !proof.verify_element_inclusion(&mut hasher, &element, POS, &root_hash2),
                "proof verification should fail with mangled root_hash"
            );
            let mut proof2 = proof.clone();
            proof2.hashes[0] = test_digest(0);
            assert!(
                !proof2.verify_element_inclusion(&mut hasher, &element, POS, &root_hash),
                "proof verification should fail with mangled proof hash"
            );
            proof2 = proof.clone();
            proof2.size = 10;
            assert!(
                !proof2.verify_element_inclusion(&mut hasher, &element, POS, &root_hash),
                "proof verification should fail with incorrect size"
            );
            proof2 = proof.clone();
            proof2.hashes.push(test_digest(0));
            assert!(
                !proof2.verify_element_inclusion(&mut hasher, &element, POS, &root_hash),
                "proof verification should fail with extra hash"
            );
            proof2 = proof.clone();
            while !proof2.hashes.is_empty() {
                proof2.hashes.pop();
                assert!(
                    !proof2.verify_element_inclusion(&mut hasher, &element, 7, &root_hash),
                    "proof verification should fail with missing hashes"
                );
            }
            proof2 = proof.clone();
            proof2.hashes.clear();
            const PEAK_COUNT: usize = 3;
            proof2
                .hashes
                .extend(proof.hashes[0..PEAK_COUNT - 1].iter().cloned());
            // sneak in an extra hash that won't be used in the computation and make sure it's detected
            proof2.hashes.push(test_digest(0));
            proof2
                .hashes
                .extend(proof.hashes[PEAK_COUNT - 1..].iter().cloned());
            assert!(
            !proof2.verify_element_inclusion(&mut hasher, &element, POS, &root_hash),
            "proof verification should fail with extra hash even if it's unused by the computation"
        );
        });
    }

    #[test]
    fn test_verification_verify_range() {
        let executor = deterministic::Runner::default();

        executor.start(|_| async move {
            // create a new MMR and add a non-trivial amount (49) of elements
            let mut mmr: Mmr<Sha256> = Mmr::default();
            let mut elements = Vec::new();
            let mut element_positions = Vec::<u64>::new();
            let mut hasher = Sha256::default();
            for i in 0..49 {
                elements.push(test_digest(i));
                element_positions.push(mmr.add(&mut hasher, elements.last().unwrap()));
            }
            // test range proofs over all possible ranges of at least 2 elements
            let root_hash = mmr.root(&mut hasher);
            let mut hasher = Sha256::default();

            for i in 0..elements.len() {
                for j in i + 1..elements.len() {
                    let start_pos = element_positions[i];
                    let end_pos = element_positions[j];
                    let range_proof = mmr.range_proof(start_pos, end_pos).await.unwrap();
                    assert!(
                        range_proof.verify_range_inclusion(
                            &mut hasher,
                            &elements[i..j + 1],
                            start_pos,
                            end_pos,
                            &root_hash,
                        ),
                        "valid range proof should verify successfully {}:{}",
                        i,
                        j
                    );
                }
            }

            // create a test range for which we will mangle data and confirm the proof fails
            let start_index = 33;
            let end_index = 39;
            let start_pos = element_positions[start_index];
            let end_pos = element_positions[end_index];
            let range_proof = mmr.range_proof(start_pos, end_pos).await.unwrap();
            let valid_elements = &elements[start_index..end_index + 1];
            assert!(
                range_proof.verify_range_inclusion(
                    &mut hasher,
                    valid_elements,
                    start_pos,
                    end_pos,
                    &root_hash,
                ),
                "valid range proof should verify successfully"
            );
            let mut invalid_proof = range_proof.clone();
            for _i in 0..range_proof.hashes.len() {
                invalid_proof.hashes.remove(0);
                assert!(
                    !range_proof.verify_range_inclusion(
                        &mut hasher,
                        Vec::<&[u8]>::new(),
                        start_pos,
                        end_pos,
                        &root_hash,
                    ),
                    "range proof with removed elements should fail"
                );
            }
            // confirm proof fails with invalid element hashes
            for i in 0..elements.len() {
                for j in i..elements.len() {
                    if i == start_index && j == end_index {
                        // skip the valid range
                        continue;
                    }
                    assert!(
                        !range_proof.verify_range_inclusion(
                            &mut hasher,
                            &elements[i..j + 1],
                            start_pos,
                            end_pos,
                            &root_hash,
                        ),
                        "range proof with invalid elements should fail {}:{}",
                        i,
                        j
                    );
                }
            }
            // confirm proof fails with invalid root hash
            let invalid_root_hash = test_digest(1);
            assert!(
                !range_proof.verify_range_inclusion(
                    &mut hasher,
                    valid_elements,
                    start_pos,
                    end_pos,
                    &invalid_root_hash,
                ),
                "range proof with invalid root hash should fail"
            );
            // mangle the proof and confirm it fails
            let mut invalid_proof = range_proof.clone();
            invalid_proof.hashes[1] = test_digest(0);
            assert!(
                !invalid_proof.verify_range_inclusion(
                    &mut hasher,
                    valid_elements,
                    start_pos,
                    end_pos,
                    &root_hash,
                ),
                "mangled range proof should fail verification"
            );
            // inserting elements into the proof should also cause it to fail (malleability check)
            for i in 0..range_proof.hashes.len() {
                let mut invalid_proof = range_proof.clone();
                invalid_proof.hashes.insert(i, test_digest(0));
                assert!(
                    !invalid_proof.verify_range_inclusion(
                        &mut hasher,
                        valid_elements,
                        start_pos,
                        end_pos,
                        &root_hash,
                    ),
                    "mangled range proof should fail verification. inserted element at: {}",
                    i
                );
            }
            // removing proof elements should cause verification to fail
            let mut invalid_proof = range_proof.clone();
            for _ in 0..range_proof.hashes.len() {
                invalid_proof.hashes.remove(0);
                assert!(
                    !invalid_proof.verify_range_inclusion(
                        &mut hasher,
                        valid_elements,
                        start_pos,
                        end_pos,
                        &root_hash,
                    ),
                    "shortened range proof should fail verification"
                );
            }
            // bad element range should cause verification to fail
            for i in 0..elements.len() {
                for j in 0..elements.len() {
                    let start_pos2 = element_positions[i];
                    let end_pos2 = element_positions[j];
                    if start_pos2 == start_pos && end_pos2 == end_pos {
                        continue;
                    }
                    assert!(
                        !range_proof.verify_range_inclusion(
                            &mut hasher,
                            valid_elements,
                            start_pos2,
                            end_pos2,
                            &root_hash,
                        ),
                        "bad element range should fail verification {}:{}",
                        i,
                        j
                    );
                }
            }
        });
    }

    #[test]
    fn test_verification_retained_nodes_provable_after_pruning() {
        let executor = deterministic::Runner::default();
        executor.start(|_| async move {
            // create a new MMR and add a non-trivial amount (49) of elements
            let mut mmr: Mmr<Sha256> = Mmr::default();
            let mut elements = Vec::<Digest>::new();
            let mut element_positions = Vec::<u64>::new();
            let mut hasher = Sha256::default();
            for i in 0..49 {
                elements.push(test_digest(i));
                element_positions.push(mmr.add(&mut hasher, elements.last().unwrap()));
            }

            // Confirm we can successfully prove all retained elements in the MMR after pruning.
            let root = mmr.root(&mut hasher);
            for i in 1..mmr.size() {
                mmr.prune_to_pos(i);
                let pruned_root = mmr.root(&mut hasher);
                assert_eq!(root, pruned_root);
                for (j, pos) in element_positions.iter().enumerate() {
                    let proof = mmr.proof(*pos).await;
                    if *pos < i {
                        assert!(proof.is_err());
                    } else {
                        assert!(proof.is_ok());
                        assert!(proof.unwrap().verify_element_inclusion(
                            &mut hasher,
                            &elements[j],
                            *pos,
                            &root
                        ));
                    }
                }
            }
        });
    }

    #[test]
    fn test_verification_ranges_provable_after_pruning() {
        let executor = deterministic::Runner::default();
        executor.start(|_| async move {
            // create a new MMR and add a non-trivial amount (49) of elements
            let mut mmr: Mmr<Sha256> = Mmr::default();
            let mut elements = Vec::<Digest>::new();
            let mut element_positions = Vec::<u64>::new();
            let mut hasher = Sha256::default();
            for i in 0..49 {
                elements.push(test_digest(i));
                element_positions.push(mmr.add(&mut hasher, elements.last().unwrap()));
            }

            // prune up to the first peak
            mmr.prune_to_pos(62);
            assert_eq!(mmr.oldest_retained_pos().unwrap(), 62);
            for i in 0..elements.len() {
                if element_positions[i] > 62 {
                    elements = elements[i..elements.len()].to_vec();
                    element_positions = element_positions[i..element_positions.len()].to_vec();
                    break;
                }
            }

            // test range proofs over all possible ranges of at least 2 elements
            let root_hash = mmr.root(&mut hasher);
            let mut hasher = Sha256::default();
            for i in 0..elements.len() {
                for j in i + 1..elements.len() {
                    let start_pos = element_positions[i];
                    let end_pos = element_positions[j];
                    let range_proof = mmr.range_proof(start_pos, end_pos).await.unwrap();
                    assert!(
                        range_proof.verify_range_inclusion(
                            &mut hasher,
                            &elements[i..j + 1],
                            start_pos,
                            end_pos,
                            &root_hash,
                        ),
                        "valid range proof over remaining elements should verify successfully",
                    );
                }
            }

            // add a few more nodes, prune again, and test again to make sure repeated pruning works
            for i in 0..37 {
                elements.push(test_digest(i));
                element_positions.push(mmr.add(&mut hasher, elements.last().unwrap()));
            }
            mmr.prune_to_pos(130); // a bit after the new highest peak
            assert_eq!(mmr.oldest_retained_pos().unwrap(), 130);

            let updated_root_hash = mmr.root(&mut hasher);
            for i in 0..elements.len() {
                if element_positions[i] >= 130 {
                    elements = elements[i..elements.len()].to_vec();
                    element_positions = element_positions[i..element_positions.len()].to_vec();
                    break;
                }
            }
            let start_pos = element_positions[0];
            let end_pos = *element_positions.last().unwrap();
            let range_proof = mmr.range_proof(start_pos, end_pos).await.unwrap();
            assert!(
                range_proof.verify_range_inclusion(
                    &mut hasher,
                    elements,
                    start_pos,
                    end_pos,
                    &updated_root_hash,
                ),
                "valid range proof over remaining elements after 2 pruning rounds should verify successfully",
            );
        });
    }

    #[test]
    fn test_verification_proof_serialization() {
        let executor = deterministic::Runner::default();
        executor.start(|_| async move {
            assert_eq!(
                Proof::<Sha256>::max_serialization_size(),
                8168,
                "wrong max serialization size of a Sha256 proof"
            );
            // create a new MMR and add a non-trivial amount of elements
            let mut mmr: Mmr<Sha256> = Mmr::default();
            let mut elements = Vec::<Digest>::new();
            let mut element_positions = Vec::<u64>::new();
            let mut hasher = Sha256::default();
            for i in 0..25 {
                elements.push(test_digest(i));
                element_positions.push(mmr.add(&mut hasher, elements.last().unwrap()));
            }

            // Generate proofs over all possible ranges of elements and confirm each
            // serializes=>deserializes correctly.
            for i in 0..elements.len() {
                for j in i..elements.len() {
                    let start_pos = element_positions[i];
                    let end_pos = element_positions[j];
                    let proof = mmr.range_proof(start_pos, end_pos).await.unwrap();

                    let mut serialized_proof = proof.serialize();
                    let deserialized_proof = Proof::<Sha256>::deserialize(&serialized_proof);
                    assert!(deserialized_proof.is_some(), "proof didn't deserialize");
                    assert_eq!(
                        proof,
                        deserialized_proof.unwrap(),
                        "deserialized proof should match source proof"
                    );

                    let deserialized_truncated = Proof::<Sha256>::deserialize(
                        &serialized_proof[0..serialized_proof.len() - 1],
                    );
                    assert!(
                        deserialized_truncated.is_none(),
                        "proof should not deserialize with truncated data"
                    );

                    serialized_proof.push(i as u8);
                    assert!(
                        Proof::<Sha256>::deserialize(&serialized_proof).is_none(),
                        "proof should not deserialize with extra data"
                    );
                }
            }
        });
    }
}
