// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Classify suppression candidates using Clang token ranges, never a C++
//! parser.
use crate::error::Error;
use crate::error::config;
use crate::native_token::NativeToken;

pub(crate) fn positions(original: &str, tokens: &[NativeToken]) -> Result<Vec<[usize; 6]>, Error> {
    let (logical, offsets) = splice_lines(original);
    let mut result = Vec::new();
    for (index, window) in logical.windows(6).enumerate() {
        if window != b"NOLINT" {
            continue;
        }
        let mut span = [0; 6];
        span.copy_from_slice(&offsets[index..index + 6]);
        let token = tokens
            .iter()
            .find(|token| token.start <= span[0] && token.end > span[5])
            .ok_or_else(|| {
                config(format!(
                    "cannot safely classify NOLINT at byte {}: Clang supplied no containing token",
                    span[0]
                ))
            })?;
        match token.kind {
            4 => result.push(span),
            2 => {}
            3 => {
                if ambiguous_header_literal(original, &logical, &offsets, tokens, token) {
                    return Err(config(format!(
                        "cannot safely classify NOLINT at byte {}: raw literal overlaps a preprocessing/header-name context",
                        span[0]
                    )));
                }
            }
            _ => {
                return Err(config(format!(
                    "cannot safely classify NOLINT at byte {}: unsupported Clang token kind {}",
                    span[0], token.kind
                )));
            }
        }
    }
    Ok(result)
}

fn ambiguous_header_literal(
    original: &str,
    logical: &[u8],
    offsets: &[usize],
    tokens: &[NativeToken],
    literal: &NativeToken,
) -> bool {
    let logical_start = offsets.partition_point(|offset| *offset < literal.start);
    let line_start = logical[..logical_start]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |last| last + 1);
    let previous: Vec<_> = tokens
        .iter()
        .filter(|token| {
            token.kind != 4
                && token.end <= literal.start
                && offsets.partition_point(|offset| *offset < token.start) >= line_start
        })
        .collect();
    let spellings: Vec<_> = previous
        .iter()
        .map(|token| splice_lines(&original[token.start..token.end]).0)
        .collect();
    let first = spellings.first().map(Vec::as_slice).unwrap_or(b"");
    let second = spellings.get(1).map(Vec::as_slice).unwrap_or(b"");
    let third = spellings.get(2).map(Vec::as_slice).unwrap_or(b"");
    let trigraph = first == b"?" && second == b"?" && third == b"=";
    let marker = [b"#".as_slice(), b"%:".as_slice(), b"??=".as_slice()].contains(&first) || trigraph;
    if marker {
        let directive = if trigraph {
            spellings.get(3).map(Vec::as_slice).unwrap_or(b"")
        } else {
            second
        };
        if [b"define".as_slice(), b"line".as_slice()].contains(&directive) {
            return false;
        }
        let keyword_index = if trigraph { 3 } else { 1 };
        if directive == b"pragma" && spellings.get(keyword_index + 1).is_some_and(|word| word == b"message") {
            return false;
        }
        let literal_bytes = &original.as_bytes()[literal.start..literal.end];
        let ordinary_quoted = literal_bytes.starts_with(b"\"")
            && literal_bytes.ends_with(b"\"")
            && !literal_bytes.contains(&b'\\')
            && !literal_bytes.contains(&b'\n')
            && !literal_bytes.windows(2).any(|pair| pair == b"??");
        // A plain quoted token immediately following an include directive or
        // query '(' has the same boundary in header-name and string-literal
        // lexing. Quotes inside '<...>' and raw literals remain uncertified.
        let last = spellings.last().map(Vec::as_slice).unwrap_or(b"");
        if ordinary_quoted
            && (last == b"("
                || [b"include".as_slice(), b"include_next".as_slice(), b"import".as_slice()].contains(&last))
        {
            return false;
        }
        return true;
    }
    spellings.last().is_some_and(|text| text == b"import")
        || spellings.windows(2).any(|pair| pair[0] == b"import" && pair[1] == b"<")
}

fn splice_lines(source: &str) -> (Vec<u8>, Vec<usize>) {
    let bytes = source.as_bytes();
    let mut logical = Vec::with_capacity(bytes.len());
    let mut offsets = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            let mut next = index + 1;
            while bytes
                .get(next)
                .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\x0b' | b'\x0c'))
            {
                next += 1;
            }
            if bytes.get(next) == Some(&b'\n') {
                index = next + 1;
                continue;
            }
            if bytes.get(next) == Some(&b'\r') && bytes.get(next + 1) == Some(&b'\n') {
                index = next + 2;
                continue;
            }
        }
        logical.push(bytes[index]);
        offsets.push(index);
        index += 1;
    }
    (logical, offsets)
}
