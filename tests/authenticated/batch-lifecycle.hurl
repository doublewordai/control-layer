# Batch lifecycle: create -> validating -> poll until in_progress
# Verifies that batch creation returns "validating" and the background worker
# transitions it to "in_progress" after populating requests.

# Upload a batch input file
POST http://localhost:3001/ai/v1/files
[Cookies]
dwctl_session: {{admin_jwt}}
[MultipartFormData]
purpose: batch
file: file,../files/gpt4.jsonl; application/x-ndjson
HTTP 201
[Captures]
file_id: jsonpath "$.id"

# Create a batch — should return "validating"
POST http://localhost:3001/ai/v1/batches
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "input_file_id": "{{file_id}}",
  "endpoint": "/v1/chat/completions",
  "completion_window": "24h"
}
HTTP 201
[Captures]
batch_id: jsonpath "$.id"
[Asserts]
jsonpath "$.status" == "validating"
jsonpath "$.output_file_id" != null
jsonpath "$.error_file_id" != null

# Poll until the worker populates the batch
GET http://localhost:3001/ai/v1/batches/{{batch_id}}
[Options]
retry: 10
retry-interval: 500
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.status" == "in_progress"
jsonpath "$.request_counts.total" > 0