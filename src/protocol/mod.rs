#![allow(dead_code)]

pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/omnimorph.cloud.v1.rs"));
}

#[cfg(test)]
mod tests;
