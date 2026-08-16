struct Params {
    text_len: u32,
    _padding_0: u32,
    _padding_1: u32,
    _padding_2: u32,
}

@group(0) @binding(0)
var<storage, read> input_words: array<u32>;

@group(0) @binding(1)
var<storage, read_write> output_words: array<u32>;

@group(0) @binding(2)
var<uniform> params: Params;

fn byte_at(index: u32) -> u32 {
    if index >= params.text_len {
        return 0u;
    }
    let word = input_words[index / 4u];
    return (word >> ((index % 4u) * 8u)) & 255u;
}

fn is_identifier(byte: u32) -> bool {
    return (byte >= 48u && byte <= 57u)
        || (byte >= 65u && byte <= 90u)
        || byte == 95u
        || (byte >= 97u && byte <= 122u);
}

fn delimited(start: u32, length: u32) -> bool {
    let before = start == 0u || !is_identifier(byte_at(start - 1u));
    let after_index = start + length;
    let after = after_index >= params.text_len || !is_identifier(byte_at(after_index));
    return before && after;
}

fn keyword_length(start: u32) -> u32 {
    if start >= params.text_len || (start > 0u && is_identifier(byte_at(start - 1u))) {
        return 0u;
    }

    let first = byte_at(start);
    if first == 101u {
        if byte_at(start + 1u) == 108u
            && byte_at(start + 2u) == 115u
            && byte_at(start + 3u) == 101u
            && delimited(start, 4u)
        {
            return 4u;
        }
        if byte_at(start + 1u) == 110u
            && byte_at(start + 2u) == 117u
            && byte_at(start + 3u) == 109u
            && delimited(start, 4u)
        {
            return 4u;
        }
    } else if first == 102u {
        if byte_at(start + 1u) == 110u && delimited(start, 2u) {
            return 2u;
        }
        if byte_at(start + 1u) == 111u
            && byte_at(start + 2u) == 114u
            && delimited(start, 3u)
        {
            return 3u;
        }
    } else if first == 105u {
        if byte_at(start + 1u) == 102u && delimited(start, 2u) {
            return 2u;
        }
        if byte_at(start + 1u) == 109u
            && byte_at(start + 2u) == 112u
            && byte_at(start + 3u) == 108u
            && delimited(start, 4u)
        {
            return 4u;
        }
    } else if first == 108u {
        if byte_at(start + 1u) == 101u
            && byte_at(start + 2u) == 116u
            && delimited(start, 3u)
        {
            return 3u;
        }
    } else if first == 109u {
        if byte_at(start + 1u) == 117u
            && byte_at(start + 2u) == 116u
            && delimited(start, 3u)
        {
            return 3u;
        }
        if byte_at(start + 1u) == 97u
            && byte_at(start + 2u) == 116u
            && byte_at(start + 3u) == 99u
            && byte_at(start + 4u) == 104u
            && delimited(start, 5u)
        {
            return 5u;
        }
    } else if first == 112u {
        if byte_at(start + 1u) == 117u
            && byte_at(start + 2u) == 98u
            && delimited(start, 3u)
        {
            return 3u;
        }
    } else if first == 114u {
        if byte_at(start + 1u) == 101u
            && byte_at(start + 2u) == 116u
            && byte_at(start + 3u) == 117u
            && byte_at(start + 4u) == 114u
            && byte_at(start + 5u) == 110u
            && delimited(start, 6u)
        {
            return 6u;
        }
    } else if first == 115u {
        if byte_at(start + 1u) == 116u
            && byte_at(start + 2u) == 114u
            && byte_at(start + 3u) == 117u
            && byte_at(start + 4u) == 99u
            && byte_at(start + 5u) == 116u
            && delimited(start, 6u)
        {
            return 6u;
        }
    } else if first == 116u {
        if byte_at(start + 1u) == 114u
            && byte_at(start + 2u) == 97u
            && byte_at(start + 3u) == 105u
            && byte_at(start + 4u) == 116u
            && delimited(start, 5u)
        {
            return 5u;
        }
    } else if first == 117u {
        if byte_at(start + 1u) == 115u
            && byte_at(start + 2u) == 101u
            && delimited(start, 3u)
        {
            return 3u;
        }
    } else if first == 119u {
        if byte_at(start + 1u) == 104u
            && byte_at(start + 2u) == 105u
            && byte_at(start + 3u) == 108u
            && byte_at(start + 4u) == 101u
            && delimited(start, 5u)
        {
            return 5u;
        }
    }
    return 0u;
}

@compute @workgroup_size(256)
fn classify(@builtin(global_invocation_id) invocation: vec3<u32>) {
    let output_index = invocation.x;
    let first_byte = output_index * 32u;
    if first_byte >= params.text_len {
        return;
    }

    var starts = 0u;
    for (var lane = 0u; lane < 32u; lane = lane + 1u) {
        let byte_index = first_byte + lane;
        if byte_index < params.text_len && keyword_length(byte_index) != 0u {
            starts = starts | (1u << lane);
        }
    }
    output_words[output_index] = starts;
}
