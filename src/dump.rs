use h3o::CellIndex;
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize)]
pub struct AP {
    pub mac: u64,
    pub cell: CellIndex,
}
