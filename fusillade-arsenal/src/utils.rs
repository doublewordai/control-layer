/// Default average custom_id length for JSONL overhead estimation.
///
/// This is a conservative estimate used when actual custom_id data isn't available.
/// Based on typical UUID format (36 chars) or short identifiers (10-20 chars).
///
/// Why 20? It's a middle ground that works well for most use cases:
/// - Short IDs (e.g., "req-123"): ~10 bytes
/// - UUIDs (e.g., "550e8400-e29b-41d4-a716-446655440000"): 36 bytes
/// - Custom strings: typically 10-30 bytes
pub const DEFAULT_AVG_CUSTOM_ID_LENGTH: usize = 20;

/// Default status code for JSONL overhead estimation.
///
/// Uses 200 (OK) as the default since:
/// - Most successful API calls return 2xx status codes
/// - 200 is 3 digits, same as most status codes (100-599)
/// - Overhead difference between codes is minimal (1-3 bytes)
pub const DEFAULT_AVG_STATUS_CODE: i16 = 200;

/// Calculate the raw size of a response body in bytes.
/// This is the actual response content size, not the JSONL-formatted size.
/// Returns None if the size exceeds i64::MAX.
pub fn calculate_response_body_size(response_body: &str) -> Option<i64> {
    i64::try_from(response_body.len()).ok()
}

/// Calculate the raw size of an error message in bytes.
/// This is the actual error content size, not the JSONL-formatted size.
/// Returns None if the size exceeds i64::MAX.
pub fn calculate_error_message_size(error_json: &str) -> Option<i64> {
    i64::try_from(error_json.len()).ok()
}

/// Calculate JSONL formatting overhead per line for output file.
/// This is the extra bytes added when wrapping a response in JSONL format.
/// Format: {"id":"batch_req_UUID","custom_id":"...","response":{"status_code":200,"body":{...}}}\n
///
/// # Overhead Breakdown
/// - Base structure: ~70 bytes (includes id, custom_id field name, response wrapper)
/// - custom_id value: Varies (included in parameter, already JSON-escaped by caller)
/// - Error margin: 50 bytes fixed overhead to account for:
///   - request_id field if present (~45 bytes)
///   - Additional JSON escaping in response body (~10% handled separately by caller)
///   - Minor variations in formatting
///
/// Note: This is an estimate. Actual overhead may vary based on:
/// - Length of custom_id (if provided, should already be JSON-escaped)
/// - Status code length (1-3 digits typically)
/// - Presence of optional fields like request_id
/// - JSON escaping requirements in the response body
pub fn calculate_jsonl_output_overhead_per_line(
    custom_id: &Option<String>,
    response_status: i16,
) -> i64 {
    // Base structure without custom_id or body
    let mut overhead = 7 +  // '{"id":"'
        10 + // 'batch_req_'
        36 + // UUID length
        2; // '",'

    // Add custom_id field (either "custom_id":"value" or "custom_id":null)
    // Note: custom_id value is assumed to be already JSON-escaped by the caller
    overhead += match custom_id {
        Some(id) => {
            // '","custom_id":"' (14 chars) + value + closing '"' (1 char)
            14 + id.len() + 1
        }
        None => {
            // '","custom_id":null' (16 chars)
            16
        }
    };

    overhead += 13 + // ',"response":{'
        14 + // '"status_code":'
        response_status.to_string().len() +
        8 +  // ',"body":'
        // Note: actual body size will be added separately by caller
        3 +  // '}}\n' (closing response, root, and newline)
        50; // Fixed error margin (see function docs for breakdown)

    overhead as i64
}

/// Calculate JSONL formatting overhead per line for error file.
/// Format: {"id":"batch_req_UUID","custom_id":"...","response":null,"error":{"code":null,"message":"..."}}\n
///
/// # Overhead Breakdown
/// - Base structure: ~100 bytes (includes id, custom_id, error wrapper)
/// - custom_id value: Varies (included in parameter, already JSON-escaped by caller)
/// - Error margin: 20 bytes fixed overhead to account for:
///   - Minor variations in formatting
///   - Additional escaping needs
///
/// Note: For error messages with extensive JSON escaping (e.g., messages with many quotes,
/// backslashes, or control characters), this may underestimate the actual size. The error
/// message size should already include JSON serialization overhead from the caller.
pub fn calculate_jsonl_error_overhead_per_line(custom_id: &Option<String>) -> i64 {
    // Base structure without custom_id or error message
    let mut overhead = 7 +  // '{"id":"'
        10 + // 'batch_req_'
        36 + // UUID length
        2; // '",'

    // Add custom_id field
    // Note: custom_id value is assumed to be already JSON-escaped by the caller
    overhead += match custom_id {
        Some(id) => {
            // '","custom_id":"' (14 chars) + value + closing '"'
            13 + id.len() + 1
        }
        None => {
            // '","custom_id":null' (16 chars)
            16
        }
    };

    overhead += 10 + // ',"response":'
        21 + // 'null,"error":{"code":'
        16 + // 'null,"message":"'
        // Note: actual error message size will be added separately by caller
        4 +  // '"}}\n'
        20; // Fixed error margin (see function docs)

    overhead as i64
}

/// Estimate total JSONL file size for output file from raw response body sizes.
/// Takes the sum of raw body sizes and adds estimated overhead per request.
///
/// Returns None if the calculation would overflow i64 (extremely unlikely in practice,
/// would require petabytes of data).
pub fn estimate_output_file_size(
    raw_body_size_sum: i64,
    request_count: i64,
    avg_custom_id_len: Option<usize>,
) -> Option<i64> {
    let avg_custom_id_len = avg_custom_id_len.unwrap_or(DEFAULT_AVG_CUSTOM_ID_LENGTH);
    let avg_status_code = DEFAULT_AVG_STATUS_CODE;

    // Average overhead per line
    let avg_overhead = calculate_jsonl_output_overhead_per_line(
        &Some("x".repeat(avg_custom_id_len)),
        avg_status_code,
    );

    // Use checked arithmetic to prevent overflow
    avg_overhead
        .checked_mul(request_count)
        .and_then(|overhead_total| raw_body_size_sum.checked_add(overhead_total))
}

/// Estimate total JSONL file size for error file from raw error message sizes.
///
/// Returns None if the calculation would overflow i64 (extremely unlikely in practice,
/// would require petabytes of data).
pub fn estimate_error_file_size(
    raw_error_size_sum: i64,
    request_count: i64,
    avg_custom_id_len: Option<usize>,
) -> Option<i64> {
    let avg_custom_id_len = avg_custom_id_len.unwrap_or(DEFAULT_AVG_CUSTOM_ID_LENGTH);

    // Average overhead per line
    let avg_overhead =
        calculate_jsonl_error_overhead_per_line(&Some("x".repeat(avg_custom_id_len)));

    // Use checked arithmetic to prevent overflow
    avg_overhead
        .checked_mul(request_count)
        .and_then(|overhead_total| raw_error_size_sum.checked_add(overhead_total))
}
