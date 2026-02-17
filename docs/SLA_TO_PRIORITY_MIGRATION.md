# SLA to Priority Terminology Migration

## Overview

This document describes the changes made to replace "SLA" (Service Level Agreement) terminology with user-friendly "Priority" terminology throughout the Doubleword Control Layer API and UI. The migration maintains full backwards compatibility while improving the user experience.

## Design Principles

1. **User-Facing Changes**: All API responses and UI displays use formatted priority labels ("Standard (24h)", "High (1h)")
2. **Internal Consistency**: Database and internal storage continue using time-based values ("24h", "1h") for OpenAI Batch API compatibility
3. **Backwards Compatibility**: API requests accept both formatted labels ("Standard (24h)", "High (1h)") AND legacy time values ("24h", "1h")
4. **API Layer Conversion**: All conversion logic happens at the API serialization/deserialization boundaries
5. **Consistent UI**: Frontend displays formatted values exactly as received from API

## API Changes

### 1. Batch API (`/ai/v1/batches`)

#### Request Format (Backwards Compatible)
The `completion_window` field accepts BOTH formats:

**New format (recommended):**
```json
{
  "input_file_id": "file-abc123",
  "endpoint": "/v1/chat/completions",
  "completion_window": "Standard (24h)"
}
```

**Legacy format (still supported):**
```json
{
  "input_file_id": "file-abc123",
  "endpoint": "/v1/chat/completions",
  "completion_window": "24h"
}
```

#### Response Format (Always Formatted Priority Labels)
```json
{
  "id": "batch-abc123",
  "completion_window": "Standard (24h)",
  "status": "completed",
  ...
}
```

**Changed:** API responses now return `"Standard (24h)"` or `"High (1h)"` instead of `"24h"` or `"1h"`

### 2. Config API (`/admin/api/v1/config`)

#### Response Format
```json
{
  "batches": {
    "enabled": true,
    "allowed_completion_windows": ["Standard (24h)", "High (1h)"]
  }
}
```

**Changed:** `allowed_completion_windows` array returns formatted priority labels instead of time values

**Before:** `["24h", "1h"]`
**After:** `["Standard (24h)", "High (1h)"]`

### 3. Tariff API (Model Pricing)

#### Response Format
When fetching model details with `include=pricing`:

```json
{
  "id": "model-123",
  "alias": "gpt-4",
  "tariffs": [
    {
      "id": "tariff-456",
      "name": "Batch Standard Pricing",
      "api_key_purpose": "batch",
      "completion_window": "Standard (24h)",
      "input_price_per_token": "0.00001",
      "output_price_per_token": "0.00003"
    }
  ]
}
```

**Changed:** `completion_window` field returns formatted priority labels

## Priority Mapping

| Formatted Label | Internal Storage | Display in UI |
|----------------|------------------|---------------|
| `"Standard (24h)"` | `"24h"` | Standard (24h) |
| `"High (1h)"` | `"1h"` | High (1h) |

## Code Implementation Details

### Backend (Rust)

#### 1. Completion Window Utility Module (dwctl/src/api/models/completion_window.rs)

**Normalization Function:**
```rust
pub fn normalize_completion_window(input: &str) -> Result<String> {
    let trimmed = input.trim();

    // Direct raw format: "24h", "1h", etc.
    if trimmed.ends_with('h') && trimmed.chars()
        .take(trimmed.len() - 1)
        .all(|c| c.is_ascii_digit()) {
        return Ok(trimmed.to_lowercase());
    }

    // Display format: "Standard (24h)", "High (1h)"
    let lower = trimmed.to_lowercase();
    if lower.starts_with("standard") {
        if let Some(time) = extract_time_from_parens(&lower) {
            return Ok(time);
        }
    } else if lower.starts_with("high") {
        if let Some(time) = extract_time_from_parens(&lower) {
            return Ok(time);
        }
    }

    Err(Error::BadRequest(format!("Invalid completion window format: '{}'", input)))
}
```

**Formatting Function:**
```rust
pub fn format_completion_window(raw: &str) -> String {
    match raw {
        "24h" => "Standard (24h)".to_string(),
        "1h" => "High (1h)".to_string(),
        _ => raw.to_string(), // Pass through unknown values
    }
}
```

#### 2. Request Deserialization (dwctl/src/api/models/batches.rs)
```rust
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct CreateBatchRequest {
    pub input_file_id: String,
    pub endpoint: String,

    #[serde(deserialize_with = "deserialize_completion_window")]
    pub completion_window: String,

    pub metadata: Option<HashMap<String, String>>,
}

fn deserialize_completion_window<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where D: serde::Deserializer<'de> {
    let s = String::deserialize(deserializer)?;
    super::completion_window::normalize_completion_window(&s)
        .map_err(serde::de::Error::custom)
}
```

#### 3. Response Serialization (dwctl/src/api/handlers/batches.rs)
```rust
use crate::api::models::completion_window::format_completion_window;

fn to_batch_response_with_email(batch: Batch, email: Option<String>) -> BatchResponse {
    BatchResponse {
        id: batch.id.to_string(),
        completion_window: format_completion_window(&batch.completion_window),
        // ... other fields
    }
}
```

#### 4. Tariff API (dwctl/src/api/models/tariffs.rs)
```rust
impl From<ModelTariff> for TariffResponse {
    fn from(tariff: ModelTariff) -> Self {
        use super::completion_window::format_completion_window;

        let completion_window = tariff.completion_window
            .map(|w| format_completion_window(&w));

        Self {
            completion_window,
            // ... other fields
        }
    }
}
```

### Frontend (TypeScript)

#### Type Definitions (dashboard/src/api/control-layer/types.ts)
```typescript
export interface Batch {
  // ...
  /** Completion window (priority): "Standard (24h)", "High (1h)" */
  completion_window: string;
}

export interface ModelTariff {
  // ...
  /** Completion window (priority): "Standard (24h)", "High (1h)" */
  completion_window?: string | null;
}
```

#### UI Display Pattern
All UI components display the formatted value directly:
```typescript
<span>{batch.completion_window}</span>  // Displays "Standard (24h)"
```

**No conversion logic needed** - the API provides the formatted display value

### Batch Submission UI

The CreateBatchModal component uses formatted priority values directly from the config API:

```tsx
const { data: config } = useConfig();
const availableWindows = useMemo(
  () => config?.batches?.allowed_completion_windows || ["Standard (24h)"],
  [config?.batches?.allowed_completion_windows],
);

// Display in dropdown
<Select value={completionWindow} onValueChange={setCompletionWindow}>
  <SelectTrigger>
    <SelectValue>{completionWindow}</SelectValue>
  </SelectTrigger>
  <SelectContent>
    {availableWindows.map((window) => (
      <SelectItem key={window} value={window}>
        {window}
      </SelectItem>
    ))}
  </SelectContent>
</Select>
```

### Tariff Name Auto-Generation

When creating batch tariffs in the model pricing UI, the tariff name is automatically set to the priority value:

```tsx
const getDefaultName = (
  purpose: TariffApiKeyPurpose | "none",
  priority?: string,
): string => {
  if (purpose === "none") return "";
  if (purpose === "batch" && priority) {
    return priority; // e.g., "Standard (24h)"
  }
  return API_KEY_PURPOSE_LABELS[purpose];
};
```

**Example:** Selecting "Standard (24h)" priority auto-populates the tariff name as "Standard (24h)"

### Tariff Display UI

The ModelTariffTable displays batch tariffs with both purpose and priority in the "API Key Purpose" column:

| API Key Purpose | Name | Priority |
|----------------|------|----------|
| Batch - Standard (24h) | Standard (24h) | Standard (24h) |
| Batch - High (1h) | High (1h) | High (1h) |
| Realtime | Realtime Pricing | N/A |

**Note:** For batch tariffs, the "API Key Purpose" column shows "Batch - {priority}" format (e.g., "Batch - Standard (24h)") to clearly indicate both the purpose and the priority level.

### Transaction Display

Batch transactions in the billing/cost management page display with the format:
- `API Batch - Standard (24h): 5 requests`
- `Frontend Batch - High (1h): 12 requests`

This format clearly shows:
1. Source (API or Frontend)
2. Transaction type (Batch)
3. Priority level
4. Request count

## Testing

### Backend Tests

All existing tests updated and passing. Key test scenarios:
- ✅ Normalization accepts "24h" → stores "24h"
- ✅ Normalization accepts "Standard (24h)" → stores "24h"
- ✅ Normalization accepts "1h" → stores "1h"
- ✅ Normalization accepts "High (1h)" → stores "1h"
- ✅ Formatting converts "24h" → "Standard (24h)"
- ✅ Formatting converts "1h" → "High (1h)"
- ✅ API responses return formatted values
- ✅ Database stores raw time values

## Migration Guide for Clients

### For API Consumers

#### ✅ Recommended Approach
Use formatted priority labels in your requests:
```bash
curl -X POST https://api.example.com/ai/v1/batches \
  -H "Content-Type: application/json" \
  -d '{
    "input_file_id": "file-abc123",
    "endpoint": "/v1/chat/completions",
    "completion_window": "Standard (24h)"
  }'
```

#### ✅ Legacy Approach (Still Works)
Continue using time values:
```bash
curl -X POST https://api.example.com/ai/v1/batches \
  -H "Content-Type: application/json" \
  -d '{
    "input_file_id": "file-abc123",
    "endpoint": "/v1/chat/completions",
    "completion_window": "24h"
  }'
```

#### ⚠️ Important: Parse Response Correctly
**All API responses now return formatted labels, not time values:**

```javascript
// ❌ DON'T: Check for time values
if (batch.completion_window === "24h") { ... }

// ✅ DO: Check for formatted labels
if (batch.completion_window === "Standard (24h)") { ... }

// ✅ ALTERNATIVE: Extract the time value
const timeMatch = batch.completion_window.match(/\((\d+h)\)/);
if (timeMatch && timeMatch[1] === "24h") { ... }
```

## Breaking Changes

### ⚠️ API Response Format Change

**This is a breaking change for clients that parse `completion_window` values.**

#### Before
```json
{"completion_window": "24h"}
```

#### After
```json
{"completion_window": "Standard (24h)"}
```

**Impact:**
- Clients checking `completion_window === "24h"` will break
- Clients checking `completion_window === "1h"` will break
- Display logic that relies on time values will break

**Mitigation:**
- Clients can send either format (backwards compatible on input)
- Update client code to check for formatted labels
- Alternatively, extract time value from parentheses using regex

## Summary of Changes

### Files Modified (Backend)
1. `dwctl/src/api/models/completion_window.rs` - **NEW** - Normalization and formatting utilities
2. `dwctl/src/api/models/mod.rs` - Added module declaration
3. `dwctl/src/api/models/batches.rs` - Custom deserializer for normalization
4. `dwctl/src/api/handlers/batches.rs` - Added import for formatting function
5. `dwctl/src/api/models/tariffs.rs` - Format completion_window in From impl
6. `dwctl/src/api/handlers/config.rs` - Format allowed_completion_windows array
7. `dwctl/src/api/handlers/deployments.rs` - Updated comments and documentation
8. `dwctl/src/db/models/tariffs.rs` - Updated comments
9. `dwctl/migrations/*.sql` - Updated migration comments

### Files Modified (Frontend)
1. `dashboard/src/api/control-layer/types.ts` - Updated type comments
2. `dashboard/src/components/modals/CreateBatchModal/CreateBatchModal.tsx` - Updated defaults, removed technical tooltip
3. `dashboard/src/components/modals/CreateBatchModal/CreateBatchModal.test.tsx` - Updated test expectations
4. `dashboard/src/components/features/batches/BatchesTable/columns.tsx` - Simplified display
5. `dashboard/src/components/features/models/ModelTariffTable/ModelTariffTable.tsx` - Updated tariff name generation, defaults
6. `dashboard/src/components/modals/UpdateModelPricingModal/UpdateModelPricingModal.tsx` - Updated fallback values
7. `dashboard/src/api/control-layer/mocks/batches.json` - Updated all mock data
8. `dashboard/src/api/control-layer/mocks/handlers.ts` - Updated allowed_completion_windows

## Backwards Compatibility Matrix

| API Endpoint | Accepts "24h" | Accepts "Standard (24h)" | Returns "24h" | Returns "Standard (24h)" |
|--------------|---------------|-------------------------|---------------|-------------------------|
| POST /batches | ✅ Yes | ✅ Yes | ❌ No | ✅ Yes |
| GET /batches/:id | N/A | N/A | ❌ No | ✅ Yes |
| GET /config | N/A | N/A | ❌ No | ✅ Yes |
| POST /models (tariffs) | ✅ Yes | ✅ Yes | ❌ No | ✅ Yes |
| GET /models/:id (tariffs) | N/A | N/A | ❌ No | ✅ Yes |

**Summary:**
- ✅ All API endpoints accept BOTH formats in requests
- ✅ All API endpoints return ONLY formatted labels in responses
- ❌ Raw time values are NEVER returned in API responses (breaking change)

## Future Considerations

1. **API Versioning**: Consider v2 API that only accepts formatted labels
2. **Database Views**: Consider creating views that format completion_window for reporting
3. **Documentation**: Update all customer-facing documentation to use priority terminology
4. **Metrics**: Update Prometheus metrics to use formatted labels in descriptions

## Conclusion

This migration successfully replaces SLA terminology with priority terminology in all user-facing interfaces while maintaining full backwards compatibility. The implementation cleanly separates internal storage (raw time values like "24h") from external API representation (formatted labels like "Standard (24h)"), with all conversion logic centralized in a dedicated utility module.
