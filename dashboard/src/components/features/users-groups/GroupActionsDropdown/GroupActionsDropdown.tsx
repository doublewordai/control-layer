import React, { useState, useRef, useEffect } from "react";
import { MoreVertical, Edit, Trash2, Users } from "lucide-react";

interface GroupActionsDropdownProps {
  groupId: string;
  onEditGroup: () => void;
  onManageGroup: () => void;
  onDeleteGroup: () => void;
}

export const GroupActionsDropdown: React.FC<GroupActionsDropdownProps> = ({
  groupId,
  onEditGroup,
  onManageGroup,
  onDeleteGroup,
}) => {
  const [isOpen, setIsOpen] = useState(false);
  const dropdownRef = useRef<HTMLDivElement>(null);

  // Everyone group uses the nil UUID and should not be editable or deletable
  const isEveryoneGroup = groupId === "00000000-0000-0000-0000-000000000000";

  // Close dropdown when clicking outside
  useEffect(() => {
    const handleClickOutside = (event: MouseEvent) => {
      if (
        dropdownRef.current &&
        !dropdownRef.current.contains(event.target as Node)
      ) {
        setIsOpen(false);
      }
    };

    document.addEventListener("mousedown", handleClickOutside);
    return () => {
      document.removeEventListener("mousedown", handleClickOutside);
    };
  }, []);

  const handleAction = (action: () => void) => {
    action();
    setIsOpen(false);
  };

  // Don't render anything for the Everyone group
  if (isEveryoneGroup) {
    return null;
  }

  return (
    <div className="relative" ref={dropdownRef}>
      <button
        onClick={() => setIsOpen(!isOpen)}
        className="p-1 hover:bg-gray-100 rounded transition-colors"
        aria-label="Open menu"
        aria-expanded={isOpen}
        aria-haspopup="menu"
      >
        <MoreVertical className="w-4 h-4 text-doubleword-neutral-400" />
      </button>

      {isOpen && (
        <div
          className="absolute right-0 mt-1 w-48 bg-white rounded-lg shadow-lg border border-gray-200 py-1 z-50"
          role="menu"
          aria-label="Group actions"
        >
          <button
            onClick={() => handleAction(onEditGroup)}
            className="w-full flex items-center gap-2 px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 transition-colors"
            role="menuitem"
          >
            <Edit className="w-4 h-4" />
            Edit Group
          </button>
          <button
            onClick={() => handleAction(onManageGroup)}
            className="w-full flex items-center gap-2 px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 transition-colors"
            role="menuitem"
          >
            <Users className="w-4 h-4" />
            Manage Members
          </button>
          <button
            onClick={() => handleAction(onDeleteGroup)}
            className="w-full flex items-center gap-2 px-3 py-2 text-sm text-red-600 hover:bg-red-50 transition-colors"
            role="menuitem"
          >
            <Trash2 className="w-4 h-4" />
            Delete Group
          </button>
        </div>
      )}
    </div>
  );
};
