import React, { useState, useRef, useEffect } from "react";
import { MoreVertical, Settings, Trash2, Edit } from "lucide-react";

interface UserActionsDropdownProps {
  onEditUser: () => void;
  onManageGroups: () => void;
  onDeleteUser: () => void;
}

export const UserActionsDropdown: React.FC<UserActionsDropdownProps> = ({
  onEditUser,
  onManageGroups,
  onDeleteUser,
}) => {
  const [isOpen, setIsOpen] = useState(false);
  const dropdownRef = useRef<HTMLDivElement>(null);

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

  return (
    <div className="relative" ref={dropdownRef}>
      <button
        onClick={() => setIsOpen(!isOpen)}
        className="p-1 hover:bg-gray-100 rounded transition-colors"
      >
        <MoreVertical className="w-4 h-4 text-doubleword-neutral-400" />
      </button>

      {isOpen && (
        <div className="absolute right-0 mt-1 w-48 bg-white rounded-lg shadow-lg border border-gray-200 py-1 z-50">
          <button
            onClick={() => handleAction(onEditUser)}
            className="w-full flex items-center gap-2 px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 transition-colors"
          >
            <Edit className="w-4 h-4" />
            Edit User
          </button>
          <button
            onClick={() => handleAction(onManageGroups)}
            className="w-full flex items-center gap-2 px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 transition-colors"
          >
            <Settings className="w-4 h-4" />
            Manage Groups
          </button>
          <button
            onClick={() => handleAction(onDeleteUser)}
            className="w-full flex items-center gap-2 px-3 py-2 text-sm text-red-600 hover:bg-red-50 transition-colors"
          >
            <Trash2 className="w-4 h-4" />
            Delete User
          </button>
        </div>
      )}
    </div>
  );
};
