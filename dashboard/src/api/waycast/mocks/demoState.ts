/**
 * Demo mode state management
 * Provides localStorage-backed state for demo mode to persist changes
 */

const STORAGE_KEY = "demo-mode-state";

interface DemoState {
  modelsGroups: Record<string, string[]>; // modelId -> groupIds[]
  userGroups: Record<string, string[]>; // userId -> groupIds[]
}

/**
 * Load demo state from localStorage, falling back to initial data
 */
export function loadDemoState(
  initialModelsGroups: Record<string, string[]>,
  initialUserGroups: Record<string, string[]>,
): DemoState {
  const stored = localStorage.getItem(STORAGE_KEY);

  if (stored) {
    try {
      return JSON.parse(stored);
    } catch {
      console.warn("Failed to parse demo state, using initial data");
    }
  }

  return {
    modelsGroups: { ...initialModelsGroups },
    userGroups: { ...initialUserGroups },
  };
}

/**
 * Save demo state to localStorage
 */
export function saveDemoState(state: DemoState): void {
  localStorage.setItem(STORAGE_KEY, JSON.stringify(state));
}

/**
 * Reset demo state to initial data
 */
export function resetDemoState(): void {
  localStorage.removeItem(STORAGE_KEY);
}

/**
 * Add a model to a group
 */
export function addModelToGroup(
  state: DemoState,
  modelId: string,
  groupId: string,
): DemoState {
  const newState = {
    ...state,
    modelsGroups: { ...state.modelsGroups },
  };

  if (!newState.modelsGroups[modelId]) {
    newState.modelsGroups[modelId] = [];
  }

  if (!newState.modelsGroups[modelId].includes(groupId)) {
    newState.modelsGroups[modelId] = [
      ...newState.modelsGroups[modelId],
      groupId,
    ];
  }

  saveDemoState(newState);
  return newState;
}

/**
 * Remove a model from a group
 */
export function removeModelFromGroup(
  state: DemoState,
  modelId: string,
  groupId: string,
): DemoState {
  const newState = {
    ...state,
    modelsGroups: { ...state.modelsGroups },
  };

  if (newState.modelsGroups[modelId]) {
    newState.modelsGroups[modelId] = newState.modelsGroups[modelId].filter(
      (id) => id !== groupId,
    );
  }

  saveDemoState(newState);
  return newState;
}

/**
 * Add a user to a group
 */
export function addUserToGroup(
  state: DemoState,
  userId: string,
  groupId: string,
): DemoState {
  const newState = {
    ...state,
    userGroups: { ...state.userGroups },
  };

  if (!newState.userGroups[userId]) {
    newState.userGroups[userId] = [];
  }

  if (!newState.userGroups[userId].includes(groupId)) {
    newState.userGroups[userId] = [...newState.userGroups[userId], groupId];
  }

  saveDemoState(newState);
  return newState;
}

/**
 * Remove a user from a group
 */
export function removeUserFromGroup(
  state: DemoState,
  userId: string,
  groupId: string,
): DemoState {
  const newState = {
    ...state,
    userGroups: { ...state.userGroups },
  };

  if (newState.userGroups[userId]) {
    newState.userGroups[userId] = newState.userGroups[userId].filter(
      (id) => id !== groupId,
    );
  }

  saveDemoState(newState);
  return newState;
}
