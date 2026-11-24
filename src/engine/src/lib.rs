#![no_std]
extern crate alloc;

pub mod frontmatter;
pub mod json;
pub mod markdown;
pub mod nfkd_table;
pub mod simd;
pub mod site;
pub mod template;
pub mod text;

use alloc::vec::Vec;
use json::parse_cases_json;
use site::build_site_into_json;

pub fn process_cases_json(input: &[u8]) -> Result<Vec<u8>, ()> {
    let cases = parse_cases_json(input)?;
    let mut out = Vec::with_capacity(input.len() * 3 + 65536);
    out.push(b'[');
    for (i, c) in cases.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        build_site_into_json(c, &mut out);
    }
    out.push(b']');
    Ok(out)
}
