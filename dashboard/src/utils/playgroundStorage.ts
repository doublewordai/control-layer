/**
 * playgroundStorage.ts
 *
 * Manages persistent storage of playground conversations in localStorage.
 * Provides CRUD operations for conversations and handles schema versioning.
 */

// Re-export types from Playground for consistency
export interface ImageContent {
  type: "image_url";
  image_url: {
    url: string;
  };
}

export interface TextContent {
  type: "text";
  text: string;
}

export type MessageContent = string | (TextContent | ImageContent)[];

export interface Message {
  role: "user" | "assistant" | "system";
  content: MessageContent;
  timestamp: Date;
  modelAlias: string; // Track which model generated this message (for assistant messages)
}

export interface Conversation {
  id: string;
  currentModelAlias: string; // Current model being used, but can be changed mid-conversation
  title: string;
  createdAt: string; // ISO timestamp
  updatedAt: string; // ISO timestamp
  messages: Message[];
}

export interface ConversationStore {
  conversations: Conversation[];
  activeConversationId: string | null;
  version: number;
}

// Constants
const STORAGE_KEY = "playground-conversations";
const CURRENT_VERSION = 1;
const MAX_CONVERSATIONS = 100; // Prevent localStorage from growing too large

/**
 * Generate a simple UUID v4
 */
function generateId(): string {
  return "xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx".replace(/[xy]/g, (c) => {
    const r = (Math.random() * 16) | 0;
    const v = c === "x" ? r : (r & 0x3) | 0x8;
    return v.toString(16);
  });
}

/**
 * Generate a conversation title from the first user message
 */
function generateTitle(messages: Message[]): string {
  const now = new Date();
  const dateString = now.toLocaleDateString(undefined, {
    month: 'short',
    day: 'numeric'
  });
  const timeString = now.toLocaleTimeString(undefined, {
    hour: '2-digit',
    minute: '2-digit',
    hour12: false
  });

  const firstUserMessage = messages.find((m) => m.role === "user");

  if (!firstUserMessage) {
    // No message yet: just date and timestamp
    return `${dateString} ${timeString}`;
  }

  // Extract text from message content
  let text = "";
  if (typeof firstUserMessage.content === "string") {
    text = firstUserMessage.content;
  } else {
    const textContent = firstUserMessage.content.find((c) => c.type === "text") as TextContent | undefined;
    text = textContent?.text || "";
  }

  // Truncate to 20 characters
  const truncatedText = text.length > 20 ? text.substring(0, 20) + "..." : text;

  // Format: "Jan 15 14:34 - First message..."
  return `${dateString} ${timeString} - ${truncatedText}`;
}

/**
 * Get the conversation store from localStorage
 */
function getStore(): ConversationStore {
  try {
    const data = localStorage.getItem(STORAGE_KEY);
    if (!data) {
      return {
        conversations: [],
        activeConversationId: null,
        version: CURRENT_VERSION,
      };
    }

    const store = JSON.parse(data) as ConversationStore;

    // Handle version migrations here if needed
    if (store.version < CURRENT_VERSION) {
      return migrateStore(store);
    }

    // Parse Date objects from timestamps
    store.conversations = store.conversations.map((conv) => ({
      ...conv,
      messages: conv.messages.map((msg) => ({
        ...msg,
        timestamp: new Date(msg.timestamp),
      })),
    }));

    return store;
  } catch (error) {
    console.error("Failed to load conversation store:", error);
    return {
      conversations: [],
      activeConversationId: null,
      version: CURRENT_VERSION,
    };
  }
}

/**
 * Save the conversation store to localStorage
 */
function saveStore(store: ConversationStore): void {
  try {
    // Serialize Date objects to ISO strings
    const serializedStore = {
      ...store,
      conversations: store.conversations.map((conv) => ({
        ...conv,
        messages: conv.messages.map((msg) => ({
          ...msg,
          timestamp: msg.timestamp.toISOString ? msg.timestamp.toISOString() : msg.timestamp,
        })),
      })),
    };

    localStorage.setItem(STORAGE_KEY, JSON.stringify(serializedStore));
  } catch (error) {
    console.error("Failed to save conversation store:", error);

    // If quota exceeded, try to cleanup old conversations
    if (error instanceof DOMException && error.name === "QuotaExceededError") {
      console.warn("localStorage quota exceeded, cleaning up old conversations");
      cleanupOldConversations(store);
      // Try again after cleanup
      try {
        localStorage.setItem(STORAGE_KEY, JSON.stringify(store));
      } catch (retryError) {
        console.error("Failed to save even after cleanup:", retryError);
      }
    }
  }
}

/**
 * Migrate store from old version to current version
 */
function migrateStore(store: ConversationStore): ConversationStore {
  // Currently no migrations needed
  // In the future, handle version upgrades here
  return {
    ...store,
    version: CURRENT_VERSION,
  };
}

/**
 * Remove oldest conversations to free up space
 */
function cleanupOldConversations(store: ConversationStore): void {
  // Sort by updatedAt and keep only the 50 most recent
  store.conversations.sort((a, b) =>
    new Date(b.updatedAt).getTime() - new Date(a.updatedAt).getTime()
  );
  store.conversations = store.conversations.slice(0, 50);
}

// ============================================================================
// Public API
// ============================================================================

/**
 * Get all conversations, optionally filtered by current model
 */
export function getConversations(currentModelAlias?: string): Conversation[] {
  const store = getStore();

  if (currentModelAlias) {
    return store.conversations.filter((c) => c.currentModelAlias === currentModelAlias);
  }

  return store.conversations;
}

/**
 * Get a specific conversation by ID
 */
export function getConversation(id: string): Conversation | null {
  const store = getStore();
  return store.conversations.find((c) => c.id === id) || null;
}

/**
 * Get the active conversation ID
 */
export function getActiveConversationId(): string | null {
  const store = getStore();
  return store.activeConversationId;
}

/**
 * Set the active conversation ID
 */
export function setActiveConversationId(id: string | null): void {
  const store = getStore();
  store.activeConversationId = id;
  saveStore(store);
}

/**
 * Create a new conversation
 */
export function createConversation(
  currentModelAlias: string,
  messages: Message[] = [],
  title?: string
): Conversation {
  const store = getStore();

  // Enforce max conversations limit
  if (store.conversations.length >= MAX_CONVERSATIONS) {
    cleanupOldConversations(store);
  }

  const now = new Date().toISOString();
  const conversation: Conversation = {
    id: generateId(),
    currentModelAlias,
    title: title || generateTitle(messages),
    createdAt: now,
    updatedAt: now,
    messages,
  };

  store.conversations.push(conversation);
  store.activeConversationId = conversation.id;
  saveStore(store);

  return conversation;
}

/**
 * Update an existing conversation
 */
export function updateConversation(
  id: string,
  updates: Partial<Omit<Conversation, "id" | "createdAt">>,
  options: { skipTimestampUpdate?: boolean } = {}
): Conversation | null {
  const store = getStore();
  const index = store.conversations.findIndex((c) => c.id === id);

  if (index === -1) {
    return null;
  }

  const conversation = store.conversations[index];

  // Check if messages are actually being added (not just re-saving the same messages)
  const isAddingNewMessage =
    updates.messages &&
    updates.messages.length > conversation.messages.length;

  const updated: Conversation = {
    ...conversation,
    ...updates,
    id: conversation.id, // Ensure ID doesn't change
    createdAt: conversation.createdAt, // Ensure createdAt doesn't change
    // Only update timestamp if we're adding new messages or if not skipping
    updatedAt: (isAddingNewMessage || !options.skipTimestampUpdate)
      ? new Date().toISOString()
      : conversation.updatedAt,
  };

  store.conversations[index] = updated;
  saveStore(store);

  return updated;
}

/**
 * Switch the model for a conversation
 */
export function switchConversationModel(
  id: string,
  newModelAlias: string
): Conversation | null {
  return updateConversation(id, { currentModelAlias: newModelAlias });
}

/**
 * Delete a conversation
 */
export function deleteConversation(id: string): boolean {
  const store = getStore();
  const index = store.conversations.findIndex((c) => c.id === id);

  if (index === -1) {
    return false;
  }

  store.conversations.splice(index, 1);

  // If we deleted the active conversation, clear the active ID
  if (store.activeConversationId === id) {
    store.activeConversationId = null;
  }

  saveStore(store);
  return true;
}

/**
 * Add messages to a conversation
 */
export function addMessages(id: string, messages: Message[]): Conversation | null {
  const store = getStore();
  const conversation = store.conversations.find((c) => c.id === id);

  if (!conversation) {
    return null;
  }

  conversation.messages.push(...messages);
  conversation.updatedAt = new Date().toISOString();

  // Update title if this is the first user message
  if (conversation.messages.length === messages.length && conversation.title === "New Conversation") {
    conversation.title = generateTitle(conversation.messages);
  }

  saveStore(store);
  return conversation;
}

/**
 * Clear all conversations (useful for testing or reset)
 */
export function clearAllConversations(): void {
  localStorage.removeItem(STORAGE_KEY);
}

/**
 * Export all conversations as JSON
 */
export function exportConversations(): string {
  const store = getStore();
  return JSON.stringify(store.conversations, null, 2);
}

/**
 * Import conversations from JSON
 */
export function importConversations(json: string): boolean {
  try {
    const conversations = JSON.parse(json) as Conversation[];
    const store = getStore();

    // Validate structure
    if (!Array.isArray(conversations)) {
      throw new Error("Invalid format: expected array");
    }

    // Add imported conversations
    store.conversations.push(...conversations);
    saveStore(store);

    return true;
  } catch (error) {
    console.error("Failed to import conversations:", error);
    return false;
  }
}

// ============================================================================
// Streaming State Management
// ============================================================================

const STREAMING_STATE_KEY = "playground-streaming-state";

export interface StreamingState {
  conversationId: string;
  partialContent: string;
  userMessage: Message;
  modelAlias: string;
  timestamp: string;
}

/**
 * Save partial streaming content (called periodically during streaming)
 */
export function saveStreamingState(state: StreamingState): void {
  try {
    localStorage.setItem(STREAMING_STATE_KEY, JSON.stringify(state));
  } catch (error) {
    console.error("Failed to save streaming state:", error);
  }
}

/**
 * Get partial streaming state (for recovery after page refresh)
 */
export function getStreamingState(): StreamingState | null {
  try {
    const data = localStorage.getItem(STREAMING_STATE_KEY);
    if (!data) return null;
    return JSON.parse(data) as StreamingState;
  } catch (error) {
    console.error("Failed to load streaming state:", error);
    return null;
  }
}

/**
 * Clear streaming state (called when stream completes successfully)
 */
export function clearStreamingState(): void {
  localStorage.removeItem(STREAMING_STATE_KEY);
}
