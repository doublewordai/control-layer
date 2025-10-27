/**
 * Tests for playgroundStorage utility
 */
import { describe, it, expect, beforeEach, vi } from "vitest";
import * as PlaygroundStorage from "./playgroundStorage";
import type { Message } from "./playgroundStorage";

describe("PlaygroundStorage", () => {
  beforeEach(() => {
    // Clear localStorage before each test
    localStorage.clear();
    vi.clearAllMocks();
  });

  describe("Conversation CRUD Operations", () => {
    it("creates a new conversation with default title", () => {
      const conversation = PlaygroundStorage.createConversation("gpt-4o", []);

      expect(conversation.id).toBeDefined();
      expect(conversation.currentModelAlias).toBe("gpt-4o");
      expect(conversation.messages).toEqual([]);
      expect(conversation.createdAt).toBeDefined();
      expect(conversation.updatedAt).toBeDefined();
      // Title should be timestamp when no messages
      expect(conversation.title).toMatch(/\d{2}:\d{2}/);
    });

    it("creates conversation with message and generates title from first user message", () => {
      const messages: Message[] = [
        {
          role: "user",
          content: "What is the capital of France and what is its population?",
          timestamp: new Date(),
          modelAlias: "",
        },
      ];

      const conversation = PlaygroundStorage.createConversation("gpt-4o", messages);

      // Title should include date, time, and truncated message (20 chars)
      expect(conversation.title).toMatch(/\d+ \w+ \d{2}:\d{2} - What is the capital .../);
    });

    it("retrieves conversation by ID", () => {
      const created = PlaygroundStorage.createConversation("gpt-4o", []);
      const retrieved = PlaygroundStorage.getConversation(created.id);

      expect(retrieved).toBeDefined();
      expect(retrieved?.id).toBe(created.id);
      expect(retrieved?.currentModelAlias).toBe("gpt-4o");
    });

    it("returns null for non-existent conversation", () => {
      const retrieved = PlaygroundStorage.getConversation("non-existent-id");
      expect(retrieved).toBeNull();
    });

    it("updates conversation title", () => {
      const conversation = PlaygroundStorage.createConversation("gpt-4o", []);
      const updated = PlaygroundStorage.updateConversation(conversation.id, {
        title: "New Title",
      });

      expect(updated).toBeDefined();
      expect(updated?.title).toBe("New Title");
    });

    it("updates conversation messages and timestamp when adding new messages", () => {
      const conversation = PlaygroundStorage.createConversation("gpt-4o", []);
      const originalUpdatedAt = conversation.updatedAt;

      // Wait a bit to ensure timestamp difference
      vi.useFakeTimers();
      vi.advanceTimersByTime(1000);

      const newMessages: Message[] = [
        {
          role: "user",
          content: "Hello",
          timestamp: new Date(),
          modelAlias: "",
        },
      ];

      const updated = PlaygroundStorage.updateConversation(conversation.id, {
        messages: newMessages,
      });

      expect(updated?.messages).toHaveLength(1);
      expect(updated?.updatedAt).not.toBe(originalUpdatedAt);

      vi.useRealTimers();
    });

    it("does not update timestamp when skipTimestampUpdate is true", () => {
      const conversation = PlaygroundStorage.createConversation("gpt-4o", []);
      const originalUpdatedAt = conversation.updatedAt;

      // Wait a bit
      vi.useFakeTimers();
      vi.advanceTimersByTime(1000);

      const updated = PlaygroundStorage.updateConversation(
        conversation.id,
        { title: "New Title" },
        { skipTimestampUpdate: true }
      );

      expect(updated?.title).toBe("New Title");
      expect(updated?.updatedAt).toBe(originalUpdatedAt);

      vi.useRealTimers();
    });

    it("deletes a conversation", () => {
      const conversation = PlaygroundStorage.createConversation("gpt-4o", []);
      const deleted = PlaygroundStorage.deleteConversation(conversation.id);

      expect(deleted).toBe(true);
      expect(PlaygroundStorage.getConversation(conversation.id)).toBeNull();
    });

    it("returns false when deleting non-existent conversation", () => {
      const deleted = PlaygroundStorage.deleteConversation("non-existent-id");
      expect(deleted).toBe(false);
    });

    it("clears active conversation ID when deleting active conversation", () => {
      const conversation = PlaygroundStorage.createConversation("gpt-4o", []);
      PlaygroundStorage.setActiveConversationId(conversation.id);

      PlaygroundStorage.deleteConversation(conversation.id);

      expect(PlaygroundStorage.getActiveConversationId()).toBeNull();
    });
  });

  describe("Model Switching", () => {
    it("switches conversation model", () => {
      const conversation = PlaygroundStorage.createConversation("gpt-4o", []);
      const updated = PlaygroundStorage.switchConversationModel(
        conversation.id,
        "claude-3-opus"
      );

      expect(updated?.currentModelAlias).toBe("claude-3-opus");
    });
  });

  describe("Active Conversation Management", () => {
    it("sets and gets active conversation ID", () => {
      const conversation = PlaygroundStorage.createConversation("gpt-4o", []);
      PlaygroundStorage.setActiveConversationId(conversation.id);

      expect(PlaygroundStorage.getActiveConversationId()).toBe(conversation.id);
    });

    it("clears active conversation ID", () => {
      const conversation = PlaygroundStorage.createConversation("gpt-4o", []);
      PlaygroundStorage.setActiveConversationId(conversation.id);
      PlaygroundStorage.setActiveConversationId(null);

      expect(PlaygroundStorage.getActiveConversationId()).toBeNull();
    });
  });

  describe("Conversation Listing", () => {
    it("gets all conversations", () => {
      PlaygroundStorage.createConversation("gpt-4o", []);
      PlaygroundStorage.createConversation("claude-3", []);
      PlaygroundStorage.createConversation("gpt-4o", []);

      const conversations = PlaygroundStorage.getConversations();
      expect(conversations).toHaveLength(3);
    });

    it("filters conversations by model", () => {
      PlaygroundStorage.createConversation("gpt-4o", []);
      PlaygroundStorage.createConversation("claude-3", []);
      PlaygroundStorage.createConversation("gpt-4o", []);

      const filtered = PlaygroundStorage.getConversations("gpt-4o");
      expect(filtered).toHaveLength(2);
      expect(filtered.every((c) => c.currentModelAlias === "gpt-4o")).toBe(true);
    });
  });

  describe("Title Generation", () => {
    it("generates title with date, time, and message preview", () => {
      const messages: Message[] = [
        {
          role: "user",
          content: "What is TypeScript?",
          timestamp: new Date(),
          modelAlias: "",
        },
      ];

      const conversation = PlaygroundStorage.createConversation("gpt-4o", messages);

      // Should match format: "DD MMM HH:MM - Message..."
      expect(conversation.title).toMatch(/\d+ \w+ \d{2}:\d{2} - What is TypeScript\?/);
    });

    it("truncates long messages to 20 characters", () => {
      const messages: Message[] = [
        {
          role: "user",
          content: "This is a very long message that should be truncated",
          timestamp: new Date(),
          modelAlias: "",
        },
      ];

      const conversation = PlaygroundStorage.createConversation("gpt-4o", messages);

      expect(conversation.title).toMatch(/This is a very long.../);
      // Should not contain the full message
      expect(conversation.title).not.toContain("should be truncated");
    });

    it("handles array content and extracts text", () => {
      const messages: Message[] = [
        {
          role: "user",
          content: [
            { type: "text", text: "Describe this image" },
            { type: "image_url", image_url: { url: "data:image/png;base64,..." } },
          ],
          timestamp: new Date(),
          modelAlias: "",
        },
      ];

      const conversation = PlaygroundStorage.createConversation("gpt-4o", messages);

      expect(conversation.title).toMatch(/Describe this image/);
    });

    it("generates timestamp-only title when no user messages", () => {
      const messages: Message[] = [
        {
          role: "assistant",
          content: "Hello, how can I help you?",
          timestamp: new Date(),
          modelAlias: "",
        },
      ];

      const conversation = PlaygroundStorage.createConversation("gpt-4o", messages);

      // Should only have date and time, no message preview
      expect(conversation.title).toMatch(/^\d+ \w+ \d{2}:\d{2}$/);
    });
  });

  describe("localStorage Persistence", () => {
    it("persists conversations to localStorage", () => {
      PlaygroundStorage.createConversation("gpt-4o", []);

      const stored = localStorage.getItem("playground-conversations");
      expect(stored).toBeDefined();

      const parsed = JSON.parse(stored!);
      expect(parsed.conversations).toHaveLength(1);
      expect(parsed.version).toBe(1);
    });

    it("loads conversations from localStorage", () => {
      // Create and save
      PlaygroundStorage.createConversation("gpt-4o", []);

      // Clear in-memory state by creating new instance
      // (simulating page reload by calling getConversations which loads from storage)
      const conversations = PlaygroundStorage.getConversations();

      expect(conversations).toHaveLength(1);
      expect(conversations[0].currentModelAlias).toBe("gpt-4o");
    });

    it("handles missing localStorage data gracefully", () => {
      localStorage.clear();

      const conversations = PlaygroundStorage.getConversations();
      expect(conversations).toEqual([]);
    });

    it("handles corrupted localStorage data gracefully", () => {
      localStorage.setItem("playground-conversations", "invalid json");

      const conversations = PlaygroundStorage.getConversations();
      expect(conversations).toEqual([]);
    });
  });

  describe("Storage Management", () => {
    it("clears all conversations", () => {
      PlaygroundStorage.createConversation("gpt-4o", []);
      PlaygroundStorage.createConversation("claude-3", []);

      PlaygroundStorage.clearAllConversations();

      expect(PlaygroundStorage.getConversations()).toEqual([]);
      expect(localStorage.getItem("playground-conversations")).toBeNull();
    });
  });

  describe("Export/Import", () => {
    it("exports conversations as JSON", () => {
      PlaygroundStorage.createConversation("gpt-4o", []);

      const exported = PlaygroundStorage.exportConversations();
      const parsed = JSON.parse(exported);

      expect(Array.isArray(parsed)).toBe(true);
      expect(parsed).toHaveLength(1);
      expect(parsed[0].currentModelAlias).toBe("gpt-4o");
    });

    it("imports conversations from JSON", () => {
      const _conversation = PlaygroundStorage.createConversation("gpt-4o", []);
      const exported = PlaygroundStorage.exportConversations();

      PlaygroundStorage.clearAllConversations();
      expect(PlaygroundStorage.getConversations()).toHaveLength(0);

      const success = PlaygroundStorage.importConversations(exported);
      expect(success).toBe(true);
      expect(PlaygroundStorage.getConversations()).toHaveLength(1);
    });

    it("handles invalid import JSON", () => {
      const success = PlaygroundStorage.importConversations("invalid json");
      expect(success).toBe(false);
    });
  });

  describe("Streaming State Management", () => {
    it("saves streaming state to localStorage", () => {
      const userMessage: Message = {
        role: "user",
        content: "Test message",
        timestamp: new Date(),
        modelAlias: "",
      };

      const streamingState: PlaygroundStorage.StreamingState = {
        conversationId: "test-conversation-123",
        partialContent: "This is a partial response...",
        userMessage,
        modelAlias: "gpt-4o",
        timestamp: new Date().toISOString(),
      };

      PlaygroundStorage.saveStreamingState(streamingState);

      const retrieved = PlaygroundStorage.getStreamingState();
      expect(retrieved).toBeDefined();
      expect(retrieved?.conversationId).toBe("test-conversation-123");
      expect(retrieved?.partialContent).toBe("This is a partial response...");
      expect(retrieved?.modelAlias).toBe("gpt-4o");
    });

    it("returns null when no streaming state exists", () => {
      const state = PlaygroundStorage.getStreamingState();
      expect(state).toBeNull();
    });

    it("clears streaming state", () => {
      const userMessage: Message = {
        role: "user",
        content: "Test",
        timestamp: new Date(),
        modelAlias: "",
      };

      PlaygroundStorage.saveStreamingState({
        conversationId: "test-123",
        partialContent: "Partial...",
        userMessage,
        modelAlias: "gpt-4o",
        timestamp: new Date().toISOString(),
      });

      expect(PlaygroundStorage.getStreamingState()).toBeDefined();

      PlaygroundStorage.clearStreamingState();

      expect(PlaygroundStorage.getStreamingState()).toBeNull();
    });

    it("handles corrupted streaming state gracefully", () => {
      localStorage.setItem("playground-streaming-state", "invalid json");

      const state = PlaygroundStorage.getStreamingState();
      expect(state).toBeNull();
    });
  });
});
