# Cost Management API Documentation

This document describes the API endpoints that need to be implemented on the backend to support the Cost Management feature.

## Overview

The Cost Management feature allows users to:
- View their current credit balance
- View transaction history (credits purchased and debits from model executions)
- Add credits to their account
- Filter transactions by type, model, and date range

## API Endpoints

### 1. Get Credit Balance

**Endpoint:** `GET /admin/api/v1/credits/balance`

**Description:** Returns the current credit balance for the authenticated user.

**Response:**
```json
{
  "balance": 13400,
  "currency": "credits"
}
```

**Response Fields:**
- `balance` (number): Current credit balance
- `currency` (string): Currency type, e.g., "credits"

---

### 2. List Transactions

**Endpoint:** `GET /admin/api/v1/credits/transactions`

**Description:** Returns a paginated list of credit transactions (both credits and debits).

**Query Parameters:**
- `limit` (optional, number): Number of transactions to return (default: 50)
- `offset` (optional, number): Offset for pagination (default: 0)
- `type` (optional, string): Filter by transaction type ("credit" or "debit")
- `model` (optional, string): Filter by model name (only applies to debit transactions)
- `start_date` (optional, string): ISO 8601 timestamp for range start
- `end_date` (optional, string): ISO 8601 timestamp for range end

**Response:**
```json
{
  "transactions": [
    {
      "id": "txn_123",
      "type": "debit",
      "amount": 280,
      "description": "Model execution: claude-3-sonnet (Chat completion)",
      "timestamp": "2025-10-30T12:00:00Z",
      "balance_after": 13400,
      "model": "claude-3-sonnet"
    },
    {
      "id": "txn_122",
      "type": "credit",
      "amount": 1000,
      "description": "Credit purchase - Top up",
      "timestamp": "2025-10-29T10:30:00Z",
      "balance_after": 13680
    }
  ],
  "total": 150,
  "limit": 50,
  "offset": 0
}
```

**Response Fields:**
- `transactions` (array): Array of transaction objects
  - `id` (string): Unique transaction ID
  - `type` (string): "credit" or "debit"
  - `amount` (number): Transaction amount
  - `description` (string): Human-readable description
  - `timestamp` (string): ISO 8601 timestamp
  - `balance_after` (number): Balance after this transaction
  - `model` (optional, string): Model name (only for debit transactions)
- `total` (number): Total number of transactions (for pagination)
- `limit` (number): Limit used in this request
- `offset` (number): Offset used in this request

**Notes:**
- Transactions should be returned in reverse chronological order (newest first)
- The frontend will handle additional client-side filtering if needed

---

### 3. Add Credits

**Endpoint:** `POST /admin/api/v1/credits/add`

**Description:** Adds credits to the user's account (simulates a purchase).

**Request Body:**
```json
{
  "amount": 1000,
  "description": "Credit purchase - Top up"
}
```

**Request Fields:**
- `amount` (number, required): Number of credits to add
- `description` (optional, string): Description for the transaction

**Response:**
```json
{
  "transaction": {
    "id": "txn_124",
    "type": "credit",
    "amount": 1000,
    "description": "Credit purchase - Top up",
    "timestamp": "2025-10-30T14:00:00Z",
    "balance_after": 14400
  },
  "new_balance": 14400
}
```

**Response Fields:**
- `transaction` (object): The created transaction
- `new_balance` (number): Updated balance after adding credits

---

## Implementation Notes

### Transaction Types

1. **Credit Transactions**
   - Type: `"credit"`
   - Created when users purchase credits
   - Should increase the user's balance
   - Do not have an associated `model` field

2. **Debit Transactions**
   - Type: `"debit"`
   - Created automatically when models are executed
   - Should decrease the user's balance
   - Must have an associated `model` field
   - Description should include model name and operation type

### Balance Calculation

The balance should be calculated based on the transaction history:
- Start with an initial balance (or 0)
- Add credit transactions
- Subtract debit transactions
- The `balance_after` field should reflect the balance after that specific transaction

### Security Considerations

- All endpoints require authentication
- Users should only be able to view their own transactions and balance
- Rate limiting should be applied to prevent abuse
- The "add credits" endpoint should integrate with your payment system in production

### Frontend Integration

The frontend is already set up to:
- Use React Query hooks for data fetching
- Show loading states while fetching data
- Display toast notifications for success/error states
- Automatically invalidate and refetch queries after adding credits
- Work in both demo mode (with mock data) and production mode (with real API calls)

### Demo Mode

When the dashboard is in demo mode:
- API calls are NOT made
- Mock data is used instead
- Users can still test the UI functionality
- Adding credits updates local state only

To enable demo mode, users can toggle it in the Settings page.

## Testing the Integration

1. Start with the balance endpoint to verify authentication and basic functionality
2. Implement the transactions list endpoint with basic pagination
3. Add filtering support (type, model, date range)
4. Implement the add credits endpoint
5. Test the complete flow:
   - View balance
   - View transaction history
   - Filter transactions
   - Add credits and verify balance updates

## Example Transaction History

Here's an example of what a typical transaction history might look like:

```json
{
  "transactions": [
    {
      "id": "txn_18",
      "type": "debit",
      "amount": 280,
      "description": "Model execution: claude-3-sonnet (Chat completion)",
      "timestamp": "2025-10-30T18:00:00Z",
      "balance_after": 13400,
      "model": "claude-3-sonnet"
    },
    {
      "id": "txn_17",
      "type": "debit",
      "amount": 520,
      "description": "Model execution: gpt-4-turbo (Chat completion)",
      "timestamp": "2025-10-30T12:00:00Z",
      "balance_after": 13680,
      "model": "gpt-4-turbo"
    },
    {
      "id": "txn_16",
      "type": "debit",
      "amount": 110,
      "description": "Model execution: gpt-4o-mini (Embedding)",
      "timestamp": "2025-10-30T06:00:00Z",
      "balance_after": 14200,
      "model": "gpt-4o-mini"
    },
    {
      "id": "txn_11",
      "type": "credit",
      "amount": 3000,
      "description": "Credit purchase - Top up",
      "timestamp": "2025-10-25T10:00:00Z",
      "balance_after": 15380
    }
  ],
  "total": 18,
  "limit": 50,
  "offset": 0
}
```
