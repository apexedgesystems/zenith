//! Wire protocol implementations: packet codecs and framing stages
//! that the per-target transport compositions build from.

pub mod aproto;
pub mod ccsds_spp;
pub mod ccsds_tm;
pub mod slip;
