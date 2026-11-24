#![no_std]
extern crate alloc;

pub mod frontmatter;
pub mod json;
pub mod markdown;
pub mod nfkd_table;
pub mod out;
pub mod simd;
pub mod site;
pub mod template;
pub mod text;

use alloc::vec::Vec;
use json::CasesParser;
use out::ByteSink;
use site::build_site_into_json;

pub fn process_cases_streaming<S: ByteSink, R: FnMut()>(
    input: &[u8],
    out: &mut S,
    mut on_case_done: R,
) -> Result<(), ()> {
    let mut parser = CasesParser::new(input)?;
    out.push(b'[');
    let mut first = true;
    while let Some(case) = parser.next_case()? {
        if !first {
            out.push(b',');
        }
        first = false;
        build_site_into_json(&case, out);
        drop(case);
        on_case_done();
    }
    out.push(b']');
    Ok(())
}

pub fn process_cases_json(input: &[u8]) -> Result<Vec<u8>, ()> {
    let mut out = Vec::with_capacity(input.len() * 3 + 65536);
    process_cases_streaming(input, &mut out, || {})?;
    Ok(out)
}
