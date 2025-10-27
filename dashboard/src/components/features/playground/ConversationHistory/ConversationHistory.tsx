import React, { useState, useEffect } from "react";
import { X, Plus, MessageSquare } from "lucide-react";
import * as PlaygroundStorage from "../../../../utils/playgroundStorage";
import ConversationListItem from "./ConversationListItem";
import { Button } from "../../../ui/button";

interface ConversationHistoryProps {
  isOpen: boolean;
  onClose: () => void;
  currentConversationId: string | null;
  onSelectConversation: (id: string) => void;
  onNewConversation: () => void;
  currentModelAlias: string;
}

const ConversationHistory: React.FC<ConversationHistoryProps> = ({
  isOpen,
  onClose,
  currentConversationId,
  onSelectConversation,
  onNewConversation,
}) => {
  const [conversations, setConversations] = useState<PlaygroundStorage.Conversation[]>([]);

  const loadConversations = () => {
    const allConversations = PlaygroundStorage.getConversations();
    // Sort by last message time (newest first) - order never changes
    const sorted = allConversations.sort(
      (a, b) => new Date(b.updatedAt).getTime() - new Date(a.updatedAt).getTime()
    );
    setConversations(sorted);
  };

  useEffect(() => {
    loadConversations();
  }, [currentConversationId]); // Reload when conversation changes

  const handleDelete = (id: string) => {
    PlaygroundStorage.deleteConversation(id);
    loadConversations();

    // If we deleted the active conversation, create a new one
    if (id === currentConversationId) {
      onNewConversation();
    }
  };

  const handleRename = (id: string, newTitle: string) => {
    PlaygroundStorage.updateConversation(id, { title: newTitle });
    loadConversations();
  };

  if (!isOpen) return null;

  return (
    <>
      {/* Backdrop */}
      <div
        className="fixed inset-0 bg-black bg-opacity-25 z-40 lg:hidden"
        onClick={onClose}
      />

      {/* Sidebar */}
      <div className="fixed lg:static inset-y-0 left-0 w-80 bg-white border-r border-gray-200 z-50 flex flex-col">
        {/* Header */}
        <div className="flex items-center justify-between p-4 border-b border-gray-200">
          <h2 className="text-lg font-semibold text-gray-900">Conversations</h2>
          <button
            onClick={onClose}
            className="p-1 text-gray-500 hover:bg-gray-100 rounded lg:hidden"
            aria-label="Close sidebar"
          >
            <X className="w-5 h-5" />
          </button>
        </div>

        {/* New Conversation Button */}
        <div className="p-3 border-b border-gray-200">
          <Button
            onClick={onNewConversation}
            variant="outline"
            className="w-full"
          >
            <Plus className="w-4 h-4" />
            New Conversation
          </Button>
        </div>

        {/* Conversation List */}
        <div className="flex-1 overflow-y-auto p-3">
          {conversations.length === 0 ? (
            <div className="flex flex-col items-center justify-center py-12 text-center">
              <MessageSquare className="w-12 h-12 text-gray-400 mb-3" />
              <p className="text-sm text-gray-600 mb-1">No conversations yet</p>
              <p className="text-xs text-gray-500">
                Start a new conversation to begin
              </p>
            </div>
          ) : (
            <div className="space-y-2">
              {conversations.map((conversation) => (
                <ConversationListItem
                  key={conversation.id}
                  conversation={conversation}
                  isActive={conversation.id === currentConversationId}
                  onSelect={(id) => {
                    onSelectConversation(id);
                    // Close sidebar on mobile after selection
                    if (window.innerWidth < 1024) {
                      onClose();
                    }
                  }}
                  onDelete={handleDelete}
                  onRename={handleRename}
                />
              ))}
            </div>
          )}
        </div>

        {/* Footer */}
        <div className="p-3 border-t border-gray-200 bg-gray-50">
          <div className="text-xs text-gray-400 text-center">
            {conversations.length} conversation{conversations.length !== 1 ? 's' : ''}
          </div>
        </div>
      </div>
    </>
  );
};

export default ConversationHistory;
