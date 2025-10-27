import React, { useState } from "react";
import { Trash2, Edit2, Check, X } from "lucide-react";
import * as PlaygroundStorage from "../../../../utils/playgroundStorage";
import { Button } from "../../../ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "../../../ui/dialog";

interface ConversationListItemProps {
  conversation: PlaygroundStorage.Conversation;
  isActive: boolean;
  onSelect: (id: string) => void;
  onDelete: (id: string) => void;
  onRename: (id: string, newTitle: string) => void;
}

const ConversationListItem: React.FC<ConversationListItemProps> = ({
  conversation,
  isActive,
  onSelect,
  onDelete,
  onRename,
}) => {
  const [isEditing, setIsEditing] = useState(false);
  const [editedTitle, setEditedTitle] = useState(conversation.title);
  const [showDeleteConfirm, setShowDeleteConfirm] = useState(false);

  const formatRelativeTime = (timestamp: string): string => {
    const date = new Date(timestamp);
    const now = new Date();
    const diffMs = now.getTime() - date.getTime();
    const diffMins = Math.floor(diffMs / 60000);
    const diffHours = Math.floor(diffMs / 3600000);
    const diffDays = Math.floor(diffMs / 86400000);

    if (diffMins < 1) return "Just now";
    if (diffMins < 60) return `${diffMins}m ago`;
    if (diffHours < 24) return `${diffHours}h ago`;
    if (diffDays === 1) return "Yesterday";
    if (diffDays < 7) return `${diffDays}d ago`;
    return date.toLocaleDateString();
  };

  const handleRenameSubmit = () => {
    if (editedTitle.trim() && editedTitle !== conversation.title) {
      onRename(conversation.id, editedTitle.trim());
    }
    setIsEditing(false);
  };

  const handleRenameCancel = () => {
    setEditedTitle(conversation.title);
    setIsEditing(false);
  };

  const handleDeleteClick = () => {
    setShowDeleteConfirm(true);
  };

  const handleDeleteConfirm = () => {
    onDelete(conversation.id);
    setShowDeleteConfirm(false);
  };

  const handleDeleteCancel = () => {
    setShowDeleteConfirm(false);
  };

  return (
    <div
      className={`group relative p-3 rounded-lg cursor-pointer transition-colors ${
        isActive
          ? "bg-grey-50 border border-gray-500"
          : "hover:bg-gray-50 border border-transparent"
      }`}
      onClick={() => !isEditing && !showDeleteConfirm && onSelect(conversation.id)}
    >
      <div className="flex items-start justify-between gap-2">
        <div className="flex-1 min-w-0">
          {isEditing ? (
            <div className="flex items-center gap-1" onClick={(e) => e.stopPropagation()}>
              <input
                type="text"
                value={editedTitle}
                onChange={(e) => setEditedTitle(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") handleRenameSubmit();
                  if (e.key === "Escape") handleRenameCancel();
                }}
                className="flex-1 px-2 py-1 text-sm border border-gray-300 rounded focus:outline-none focus:ring-2 focus:ring-blue-500"
                autoFocus
              />
              <button
                onClick={handleRenameSubmit}
                className="p-1 text-green-600 hover:bg-green-50 rounded"
                title="Save"
              >
                <Check className="w-4 h-4" />
              </button>
              <button
                onClick={handleRenameCancel}
                className="p-1 text-gray-600 hover:bg-gray-100 rounded"
                title="Cancel"
              >
                <X className="w-4 h-4" />
              </button>
            </div>
          ) : (
            <h3 className="text-sm font-medium text-gray-900 truncate">
              {conversation.title}
            </h3>
          )}
          <div className="flex items-center gap-2 mt-1">
            <span className="text-xs text-gray-500">
              {formatRelativeTime(conversation.updatedAt)}
            </span>
            <span className="text-xs text-gray-400">•</span>
            <span className="text-xs text-gray-500 truncate">
              {conversation.currentModelAlias}
            </span>
          </div>
        </div>

        {!isEditing && !showDeleteConfirm && (
          <div className="flex items-center gap-1 opacity-0 group-hover:opacity-100 transition-opacity">
            <button
              onClick={(e) => {
                e.stopPropagation();
                setIsEditing(true);
              }}
              className="p-1 text-gray-600 hover:bg-gray-200 rounded"
              title="Rename"
            >
              <Edit2 className="w-3.5 h-3.5" />
            </button>
            <button
              onClick={(e) => {
                e.stopPropagation();
                handleDeleteClick();
              }}
              className="p-1 text-red-600 hover:bg-red-50 rounded"
              title="Delete"
            >
              <Trash2 className="w-3.5 h-3.5" />
            </button>
          </div>
        )}
      </div>

      <Dialog open={showDeleteConfirm} onOpenChange={setShowDeleteConfirm}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Delete conversation?</DialogTitle>
            <DialogDescription>
              This action cannot be undone. This will permanently delete the conversation.
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              onClick={handleDeleteCancel}
              variant="outline"
            >
              Cancel
            </Button>
            <Button
              onClick={handleDeleteConfirm}
              variant="destructive"
            >
              Delete
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
};

export default ConversationListItem;
