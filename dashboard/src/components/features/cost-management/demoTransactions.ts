/**
 * Demo Transactions for UI Demo Mode
 *
 * This file provides transaction data for the frontend when running in demo mode
 * (i.e., when users are exploring the UI without a real backend).
 *
 * For test mock data used by MSW in the test suite, see:
 * src/api/control-layer/mocks/transactions.json
 */

// Demo user IDs
import type { Transaction } from "@/api/control-layer";

export const DEMO_USERS = {
  SARAH_CHEN: "550e8400-e29b-41d4-a716-446655440001",
  JAMES_WILSON: "550e8400-e29b-41d4-a716-446655440002",
  ALEX_RODRIGUEZ: "550e8400-e29b-41d4-a716-446655440003",
  MARIA_GARCIA: "550e8400-e29b-41d4-a716-446655440004",
  DAVID_KIM: "550e8400-e29b-41d4-a716-446655440005",
  LISA_THOMPSON: "550e8400-e29b-41d4-a716-446655440006",
};

// Dummy data for transactions
export const generateDummyTransactions = (): Transaction[] => {
  let balance = 0;
  let idCounter = 1;

  const createTransaction = (
    user_id: string,
    transaction_type: "admin_grant" | "admin_removal" | "usage" | "purchase",
    amount: number,
    description: string,
    daysAgo: number,
  ): Transaction => {
    const id = String(idCounter++);

    // Update balance based on transaction type
    if (transaction_type === "admin_grant" || transaction_type === "purchase") {
      balance += amount;
    } else {
      balance -= amount;
    }

    // Use DEMO_GIFT for admin grants, otherwise use user_id
    const source_id =
      transaction_type === "admin_grant" ? "DEMO_GIFT" : user_id;

    const tx: Transaction = {
      id,
      user_id,
      transaction_type,
      amount,
      balance_after: balance,
      source_id,
      description,
      created_at: new Date(
        Date.now() - daysAgo * 24 * 60 * 60 * 1000,
      ).toISOString(),
    };

    return tx;
  };

  const transactions: Transaction[] = [
    // Sarah Chen - Heavy GPT-4 user
    createTransaction(
      DEMO_USERS.SARAH_CHEN,
      "admin_grant",
      50.0,
      "Initial funds purchase - Sarah Chen",
      30,
    ),
    createTransaction(
      DEMO_USERS.SARAH_CHEN,
      "usage",
      4.5,
      "Model execution: gpt-4o (Chat completion)",
      29,
    ),
    createTransaction(
      DEMO_USERS.SARAH_CHEN,
      "usage",
      5.2,
      "Model execution: gpt-4o (Chat completion)",
      28,
    ),
    createTransaction(
      DEMO_USERS.SARAH_CHEN,
      "usage",
      6.8,
      "Model execution: gpt-4o (Chat completion)",
      27,
    ),

    // James Wilson - Claude user
    createTransaction(
      DEMO_USERS.JAMES_WILSON,
      "admin_grant",
      30.0,
      "Initial funds purchase - James Wilson",
      26,
    ),
    createTransaction(
      DEMO_USERS.JAMES_WILSON,
      "usage",
      3.2,
      "Model execution: claude-sonnet-4 (Chat completion)",
      25,
    ),
    createTransaction(
      DEMO_USERS.JAMES_WILSON,
      "usage",
      1.25,
      "Model execution: claude-sonnet-4 (Chat completion)",
      24,
    ),
    createTransaction(
      DEMO_USERS.JAMES_WILSON,
      "usage",
      4.25,
      "Model execution: claude-sonnet-4 (Chat completion)",
      23,
    ),

    // Alex Rodriguez - Budget conscious, uses mini models
    createTransaction(
      DEMO_USERS.ALEX_RODRIGUEZ,
      "admin_grant",
      10.0,
      "Initial funds purchase - Alex Rodriguez",
      22,
    ),
    createTransaction(
      DEMO_USERS.ALEX_RODRIGUEZ,
      "usage",
      1.8,
      "Model execution: deepseek-v3 (Chat completion)",
      21,
    ),
    createTransaction(
      DEMO_USERS.ALEX_RODRIGUEZ,
      "usage",
      1.1,
      "Model execution: embedding-small (Embedding)",
      20,
    ),
    createTransaction(
      DEMO_USERS.ALEX_RODRIGUEZ,
      "usage",
      2.9,
      "Model execution: deepseek-v3 (Chat completion)",
      19,
    ),

    // Maria Garcia - Embedding specialist
    createTransaction(
      DEMO_USERS.MARIA_GARCIA,
      "admin_grant",
      20.0,
      "Initial funds purchase - Maria Garcia",
      18,
    ),
    createTransaction(
      DEMO_USERS.MARIA_GARCIA,
      "usage",
      0.95,
      "Model execution: embedding-small (Embedding)",
      17,
    ),
    createTransaction(
      DEMO_USERS.MARIA_GARCIA,
      "usage",
      1.45,
      "Model execution: embedding-small (Embedding)",
      16,
    ),
    createTransaction(
      DEMO_USERS.MARIA_GARCIA,
      "usage",
      2.3,
      "Model execution: embedding-small (Embedding)",
      15,
    ),

    // David Kim - Mixed usage
    createTransaction(
      DEMO_USERS.DAVID_KIM,
      "admin_grant",
      40.0,
      "Initial funds purchase - David Kim",
      14,
    ),
    createTransaction(
      DEMO_USERS.DAVID_KIM,
      "usage",
      5.4,
      "Model execution: gpt-4o (Chat completion)",
      13,
    ),
    createTransaction(
      DEMO_USERS.DAVID_KIM,
      "usage",
      2.1,
      "Model execution: claude-sonnet-4 (Chat completion)",
      12,
    ),
    createTransaction(
      DEMO_USERS.DAVID_KIM,
      "usage",
      2.8,
      "Model execution: claude-sonnet-4 (Chat completion)",
      11,
    ),

    // Lisa Thompson - Moderate user
    createTransaction(
      DEMO_USERS.LISA_THOMPSON,
      "admin_grant",
      25.0,
      "Initial funds purchase - Lisa Thompson",
      10,
    ),
    createTransaction(
      DEMO_USERS.LISA_THOMPSON,
      "usage",
      1.25,
      "Model execution: claude-sonnet-4 (Chat completion)",
      9,
    ),
    createTransaction(
      DEMO_USERS.LISA_THOMPSON,
      "usage",
      1.8,
      "Model execution: deepseek-v3 (Chat completion)",
      8,
    ),

    // Recent activity - mixed users
    createTransaction(
      DEMO_USERS.SARAH_CHEN,
      "admin_grant",
      30.0,
      "Account top-up - Sarah Chen",
      7,
    ),
    createTransaction(
      DEMO_USERS.SARAH_CHEN,
      "usage",
      4.5,
      "Model execution: gpt-4o (Chat completion)",
      6,
    ),
    createTransaction(
      DEMO_USERS.JAMES_WILSON,
      "usage",
      3.2,
      "Model execution: claude-sonnet-4 (Chat completion)",
      5,
    ),
    createTransaction(
      DEMO_USERS.ALEX_RODRIGUEZ,
      "usage",
      1.8,
      "Model execution: deepseek-v3 (Chat completion)",
      4,
    ),
    createTransaction(
      DEMO_USERS.MARIA_GARCIA,
      "usage",
      0.95,
      "Model execution: embedding-small (Embedding)",
      3,
    ),
    createTransaction(
      DEMO_USERS.DAVID_KIM,
      "admin_grant",
      15.0,
      "Account top-up - David Kim",
      2,
    ),
    createTransaction(
      DEMO_USERS.DAVID_KIM,
      "usage",
      5.4,
      "Model execution: gpt-4o (Chat completion)",
      1,
    ),
    createTransaction(
      DEMO_USERS.LISA_THOMPSON,
      "usage",
      2.1,
      "Model execution: claude-sonnet-4 (Chat completion)",
      0.5,
    ),
    createTransaction(
      DEMO_USERS.JAMES_WILSON,
      "usage",
      2.8,
      "Model execution: claude-sonnet-4 (Chat completion)",
      0.25,
    ),
  ];

  return transactions;
};
