
// Demo user IDs
import type {Transaction} from "@/components/features/cost-management/CostManagement/TransactionHistory.tsx";

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

  const transactions: Transaction[] = [
    // Sarah Chen - Heavy GPT-4 user
    { id: String(idCounter++), type: "credit", amount: 5000, description: "Initial credit purchase - Sarah Chen", timestamp: new Date(Date.now() - 30 * 24 * 60 * 60 * 1000).toISOString(), balance_after: balance += 5000, user_id: DEMO_USERS.SARAH_CHEN },
    { id: String(idCounter++), type: "debit", amount: 450, description: "Model execution: gpt-4-turbo (Chat completion)", timestamp: new Date(Date.now() - 29 * 24 * 60 * 60 * 1000).toISOString(), balance_after: balance -= 450, model: "gpt-4-turbo", user_id: DEMO_USERS.SARAH_CHEN },
    { id: String(idCounter++), type: "debit", amount: 520, description: "Model execution: gpt-4-turbo (Chat completion)", timestamp: new Date(Date.now() - 28 * 24 * 60 * 60 * 1000).toISOString(), balance_after: balance -= 520, model: "gpt-4-turbo", user_id: DEMO_USERS.SARAH_CHEN },
    { id: String(idCounter++), type: "debit", amount: 680, description: "Model execution: gpt-4-turbo (Chat completion)", timestamp: new Date(Date.now() - 27 * 24 * 60 * 60 * 1000).toISOString(), balance_after: balance -= 680, model: "gpt-4-turbo", user_id: DEMO_USERS.SARAH_CHEN },

    // James Wilson - Claude user
    { id: String(idCounter++), type: "credit", amount: 3000, description: "Initial credit purchase - James Wilson", timestamp: new Date(Date.now() - 26 * 24 * 60 * 60 * 1000).toISOString(), balance_after: balance += 3000, user_id: DEMO_USERS.JAMES_WILSON },
    { id: String(idCounter++), type: "debit", amount: 320, description: "Model execution: claude-3-opus (Chat completion)", timestamp: new Date(Date.now() - 25 * 24 * 60 * 60 * 1000).toISOString(), balance_after: balance -= 320, model: "claude-3-opus", user_id: DEMO_USERS.JAMES_WILSON },
    { id: String(idCounter++), type: "debit", amount: 125, description: "Model execution: claude-3-sonnet (Chat completion)", timestamp: new Date(Date.now() - 24 * 24 * 60 * 60 * 1000).toISOString(), balance_after: balance -= 125, model: "claude-3-sonnet", user_id: DEMO_USERS.JAMES_WILSON },
    { id: String(idCounter++), type: "debit", amount: 425, description: "Model execution: claude-3-opus (Chat completion)", timestamp: new Date(Date.now() - 23 * 24 * 60 * 60 * 1000).toISOString(), balance_after: balance -= 425, model: "claude-3-opus", user_id: DEMO_USERS.JAMES_WILSON },

    // Alex Rodriguez - Budget conscious, uses mini models
    { id: String(idCounter++), type: "credit", amount: 1000, description: "Initial credit purchase - Alex Rodriguez", timestamp: new Date(Date.now() - 22 * 24 * 60 * 60 * 1000).toISOString(), balance_after: balance += 1000, user_id: DEMO_USERS.ALEX_RODRIGUEZ },
    { id: String(idCounter++), type: "debit", amount: 180, description: "Model execution: gpt-4o-mini (Chat completion)", timestamp: new Date(Date.now() - 21 * 24 * 60 * 60 * 1000).toISOString(), balance_after: balance -= 180, model: "gpt-4o-mini", user_id: DEMO_USERS.ALEX_RODRIGUEZ },
    { id: String(idCounter++), type: "debit", amount: 110, description: "Model execution: gpt-4o-mini (Embedding)", timestamp: new Date(Date.now() - 20 * 24 * 60 * 60 * 1000).toISOString(), balance_after: balance -= 110, model: "gpt-4o-mini", user_id: DEMO_USERS.ALEX_RODRIGUEZ },
    { id: String(idCounter++), type: "debit", amount: 290, description: "Model execution: gpt-4o-mini (Chat completion)", timestamp: new Date(Date.now() - 19 * 24 * 60 * 60 * 1000).toISOString(), balance_after: balance -= 290, model: "gpt-4o-mini", user_id: DEMO_USERS.ALEX_RODRIGUEZ },

    // Maria Garcia - Embedding specialist
    { id: String(idCounter++), type: "credit", amount: 2000, description: "Initial credit purchase - Maria Garcia", timestamp: new Date(Date.now() - 18 * 24 * 60 * 60 * 1000).toISOString(), balance_after: balance += 2000, user_id: DEMO_USERS.MARIA_GARCIA },
    { id: String(idCounter++), type: "debit", amount: 95, description: "Model execution: text-embedding-ada-002 (Embedding)", timestamp: new Date(Date.now() - 17 * 24 * 60 * 60 * 1000).toISOString(), balance_after: balance -= 95, model: "text-embedding-ada-002", user_id: DEMO_USERS.MARIA_GARCIA },
    { id: String(idCounter++), type: "debit", amount: 145, description: "Model execution: text-embedding-ada-002 (Embedding)", timestamp: new Date(Date.now() - 16 * 24 * 60 * 60 * 1000).toISOString(), balance_after: balance -= 145, model: "text-embedding-ada-002", user_id: DEMO_USERS.MARIA_GARCIA },
    { id: String(idCounter++), type: "debit", amount: 230, description: "Model execution: gpt-4o-mini (Embedding)", timestamp: new Date(Date.now() - 15 * 24 * 60 * 60 * 1000).toISOString(), balance_after: balance -= 230, model: "gpt-4o-mini", user_id: DEMO_USERS.MARIA_GARCIA },

    // David Kim - Mixed usage
    { id: String(idCounter++), type: "credit", amount: 4000, description: "Initial credit purchase - David Kim", timestamp: new Date(Date.now() - 14 * 24 * 60 * 60 * 1000).toISOString(), balance_after: balance += 4000, user_id: DEMO_USERS.DAVID_KIM },
    { id: String(idCounter++), type: "debit", amount: 540, description: "Model execution: gpt-4-turbo (Chat completion)", timestamp: new Date(Date.now() - 13 * 24 * 60 * 60 * 1000).toISOString(), balance_after: balance -= 540, model: "gpt-4-turbo", user_id: DEMO_USERS.DAVID_KIM },
    { id: String(idCounter++), type: "debit", amount: 210, description: "Model execution: claude-3-sonnet (Chat completion)", timestamp: new Date(Date.now() - 12 * 24 * 60 * 60 * 1000).toISOString(), balance_after: balance -= 210, model: "claude-3-sonnet", user_id: DEMO_USERS.DAVID_KIM },
    { id: String(idCounter++), type: "debit", amount: 280, description: "Model execution: claude-3-sonnet (Chat completion)", timestamp: new Date(Date.now() - 11 * 24 * 60 * 60 * 1000).toISOString(), balance_after: balance -= 280, model: "claude-3-sonnet", user_id: DEMO_USERS.DAVID_KIM },

    // Lisa Thompson - Moderate user
    { id: String(idCounter++), type: "credit", amount: 2500, description: "Initial credit purchase - Lisa Thompson", timestamp: new Date(Date.now() - 10 * 24 * 60 * 60 * 1000).toISOString(), balance_after: balance += 2500, user_id: DEMO_USERS.LISA_THOMPSON },
    { id: String(idCounter++), type: "debit", amount: 125, description: "Model execution: claude-3-sonnet (Chat completion)", timestamp: new Date(Date.now() - 9 * 24 * 60 * 60 * 1000).toISOString(), balance_after: balance -= 125, model: "claude-3-sonnet", user_id: DEMO_USERS.LISA_THOMPSON },
    { id: String(idCounter++), type: "debit", amount: 180, description: "Model execution: gpt-4o-mini (Chat completion)", timestamp: new Date(Date.now() - 8 * 24 * 60 * 60 * 1000).toISOString(), balance_after: balance -= 180, model: "gpt-4o-mini", user_id: DEMO_USERS.LISA_THOMPSON },

    // Recent activity - mixed users
    { id: String(idCounter++), type: "credit", amount: 3000, description: "Credit top-up - Sarah Chen", timestamp: new Date(Date.now() - 7 * 24 * 60 * 60 * 1000).toISOString(), balance_after: balance += 3000, user_id: DEMO_USERS.SARAH_CHEN },
    { id: String(idCounter++), type: "debit", amount: 450, description: "Model execution: gpt-4-turbo (Chat completion)", timestamp: new Date(Date.now() - 6 * 24 * 60 * 60 * 1000).toISOString(), balance_after: balance -= 450, model: "gpt-4-turbo", user_id: DEMO_USERS.SARAH_CHEN },
    { id: String(idCounter++), type: "debit", amount: 320, description: "Model execution: claude-3-opus (Chat completion)", timestamp: new Date(Date.now() - 5 * 24 * 60 * 60 * 1000).toISOString(), balance_after: balance -= 320, model: "claude-3-opus", user_id: DEMO_USERS.JAMES_WILSON },
    { id: String(idCounter++), type: "debit", amount: 180, description: "Model execution: gpt-4o-mini (Chat completion)", timestamp: new Date(Date.now() - 4 * 24 * 60 * 60 * 1000).toISOString(), balance_after: balance -= 180, model: "gpt-4o-mini", user_id: DEMO_USERS.ALEX_RODRIGUEZ },
    { id: String(idCounter++), type: "debit", amount: 95, description: "Model execution: text-embedding-ada-002 (Embedding)", timestamp: new Date(Date.now() - 3 * 24 * 60 * 60 * 1000).toISOString(), balance_after: balance -= 95, model: "text-embedding-ada-002", user_id: DEMO_USERS.MARIA_GARCIA },
    { id: String(idCounter++), type: "credit", amount: 1500, description: "Credit top-up - David Kim", timestamp: new Date(Date.now() - 2 * 24 * 60 * 60 * 1000).toISOString(), balance_after: balance += 1500, user_id: DEMO_USERS.DAVID_KIM },
    { id: String(idCounter++), type: "debit", amount: 540, description: "Model execution: gpt-4-turbo (Chat completion)", timestamp: new Date(Date.now() - 1 * 24 * 60 * 60 * 1000).toISOString(), balance_after: balance -= 540, model: "gpt-4-turbo", user_id: DEMO_USERS.DAVID_KIM },
    { id: String(idCounter++), type: "debit", amount: 210, description: "Model execution: claude-3-sonnet (Chat completion)", timestamp: new Date(Date.now() - 12 * 60 * 60 * 1000).toISOString(), balance_after: balance -= 210, model: "claude-3-sonnet", user_id: DEMO_USERS.LISA_THOMPSON },
    { id: String(idCounter++), type: "debit", amount: 280, description: "Model execution: claude-3-sonnet (Chat completion)", timestamp: new Date(Date.now() - 6 * 60 * 60 * 1000).toISOString(), balance_after: balance -= 280, model: "claude-3-sonnet", user_id: DEMO_USERS.JAMES_WILSON },
  ];

  return transactions;
};
