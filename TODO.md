# Chat History Implementation Plan

## Overview
Add chat history functionality to the playground, starting with localStorage for quick iteration, then migrating to backend for full cross-device persistence.

**Architecture Decision (2025-10-24)**: User-centric conversations instead of model-centric

- Conversations are NOT tied to specific models
- Users can switch models mid-conversation
- Each assistant message tracks which model generated it
- More flexible UX (like ChatGPT) - compare models, continue conversations with different models

## Phase 1: localStorage + UI Foundation (Week 1)

### 1.1 Core Storage Layer ✅

- [x] Create `src/utils/playgroundStorage.ts` utility
  - [x] Define storage schema for conversations (user-centric)
  - [x] Implement save/load/delete functions for localStorage
  - [x] Support filtering by current model (optional)
  - [x] Handle serialization of `Message[]` with timestamps
  - [x] Add migration helper for future schema changes
  - [x] Track `modelAlias` on each assistant message

### 1.2 Playground State Management ✅

- [x] Update `Playground.tsx` to use persistent storage
  - [x] Load active conversation or most recent on mount
  - [x] Auto-save messages to localStorage on every change
  - [x] Debounce auto-save to avoid excessive writes (500ms)
  - [x] Support mid-conversation model switching
  - [x] "New conversation" on clear action

### 1.3 Multiple Conversations Support ✅

- [x] User-centric conversation architecture
  - [x] Add conversation metadata: `{ id, title, currentModelAlias, createdAt, updatedAt }`
  - [x] Auto-generate titles from first user message (truncate to ~50 chars)
  - [x] Implement conversation list management (CRUD operations)
  - [x] Add `switchConversationModel()` function
  - [x] Track which model generated each assistant message

### 1.4 History Sidebar UI

- [ ] Create `ConversationHistory.tsx` component
  - [ ] Collapsible sidebar (similar to ChatGPT)
  - [ ] List ALL conversations (not grouped by model)
  - [ ] Show conversation title + timestamp + current model badge
  - [ ] Highlight currently active conversation
  - [ ] Add hover actions: rename, delete
  - [ ] Empty state for no history

- [ ] Create `ConversationListItem.tsx` component
  - [ ] Display title with truncation
  - [ ] Relative timestamps ("2 hours ago", "Yesterday")
  - [ ] Action buttons: rename, delete
  - [ ] Confirm dialog before delete
  - [ ] Loading state when switching conversations

### 1.5 Playground UI Updates
- [ ] Update `GenerationPlayground.tsx`
  - [ ] Add sidebar toggle button (hamburger icon)
  - [ ] "New conversation" button in header
  - [ ] Update "Clear conversation" to archive instead of delete
  - [ ] Add conversation title display/edit in header
  - [ ] Responsive layout for sidebar (collapse on mobile)

### 1.6 Storage Management
- [ ] Add storage quota monitoring
  - [ ] Display storage usage in settings/profile
  - [ ] Warn when approaching localStorage limit (~5MB)
  - [ ] Auto-cleanup: delete oldest conversations when limit reached
  - [ ] "Export conversation" feature (download as JSON/Markdown)

### 1.7 Testing

- [x] Unit tests for `playgroundStorage.ts`
  - [x] Test save/load/delete operations
  - [x] Test schema migrations
  - [x] Test storage quota handling
  - [x] Test `switchConversationModel()` function
  - [x] All 36 tests passing

- [ ] Functional tests for conversation management
  - [ ] Test conversation creation and switching
  - [ ] Test persistence across page refresh
  - [ ] Test multi-model conversation isolation
  - [ ] Test rename/delete operations
  - [ ] Test auto-save behavior

---

## Phase 2: Backend Migration (Week 2-3)

### 2.1 Database Schema
- [ ] Create Postgres migrations in `application/clay/migrations/`
  - [ ] `conversations` table
    ```sql
    id (uuid, PK)
    user_id (uuid, FK to users)
    model_alias (varchar)
    title (varchar)
    created_at (timestamp)
    updated_at (timestamp)
    metadata (jsonb) -- for future extensibility
    ```
  - [ ] `conversation_messages` table
    ```sql
    id (uuid, PK)
    conversation_id (uuid, FK to conversations)
    role (varchar: user|assistant|system)
    content (jsonb) -- supports multimodal content
    timestamp (timestamp)
    created_at (timestamp)
    ```
  - [ ] Add indexes on user_id, model_alias, created_at
  - [ ] Add ON DELETE CASCADE for conversation messages

### 2.2 Rust Backend (Clay Service)
- [ ] Create `application/clay/src/models/conversation.rs`
  - [ ] Define `Conversation` struct
  - [ ] Define `ConversationMessage` struct
  - [ ] Implement database model methods (CRUD)
  - [ ] Add validation for content types

- [ ] Create `application/clay/src/handlers/conversations.rs`
  - [ ] `GET /admin/api/v1/conversations` - List user's conversations
  - [ ] `GET /admin/api/v1/conversations/:id` - Get conversation with messages
  - [ ] `POST /admin/api/v1/conversations` - Create new conversation
  - [ ] `PUT /admin/api/v1/conversations/:id` - Update (title, etc.)
  - [ ] `DELETE /admin/api/v1/conversations/:id` - Delete conversation
  - [ ] `POST /admin/api/v1/conversations/:id/messages` - Add message(s)
  - [ ] Add authentication middleware (user-scoped)
  - [ ] Add pagination for conversation list
  - [ ] Add filtering by model_alias

- [ ] Add routes to `application/clay/src/main.rs`
  - [ ] Register conversation handlers
  - [ ] Add to API router with auth guards

### 2.3 API Types & Client (Frontend)
- [ ] Update `dashboard/src/api/control-layer/types.ts`
  - [ ] Add `Conversation` interface
  - [ ] Add `ConversationMessage` interface
  - [ ] Add request/response types for all endpoints

- [ ] Update `dashboard/src/api/control-layer/client.ts`
  - [ ] Add `getConversations()`
  - [ ] Add `getConversation(id)`
  - [ ] Add `createConversation(data)`
  - [ ] Add `updateConversation(id, data)`
  - [ ] Add `deleteConversation(id)`
  - [ ] Add `addMessages(conversationId, messages)`

- [ ] Update `dashboard/src/api/control-layer/hooks.ts`
  - [ ] Add `useConversations()` hook
  - [ ] Add `useConversation(id)` hook
  - [ ] Add `useCreateConversation()` mutation
  - [ ] Add `useUpdateConversation()` mutation
  - [ ] Add `useDeleteConversation()` mutation
  - [ ] Add `useAddMessages()` mutation
  - [ ] Add query invalidation on mutations

- [ ] Update `dashboard/src/api/control-layer/keys.ts`
  - [ ] Add query key factory for conversations

### 2.4 Migration Strategy
- [ ] Create `src/utils/conversationMigration.ts`
  - [ ] Detect localStorage conversations on first load
  - [ ] Show migration prompt to user
  - [ ] Batch upload localStorage conversations to backend
  - [ ] Mark localStorage as "migrated" to avoid re-prompting
  - [ ] Keep localStorage as temporary cache/backup

- [ ] Update `ConversationHistory.tsx`
  - [ ] Replace localStorage calls with API hooks
  - [ ] Add loading states for API operations
  - [ ] Add error handling with retry logic
  - [ ] Show migration banner if localStorage data detected

### 2.5 Sync Strategy
- [ ] Implement optimistic updates
  - [ ] Update UI immediately, sync to backend async
  - [ ] Rollback on API failure
  - [ ] Show sync status indicator (synced/syncing/failed)

- [ ] Add offline support (optional)
  - [ ] Queue mutations when offline
  - [ ] Sync when connection restored
  - [ ] Conflict resolution for concurrent edits

### 2.6 Backend Testing
- [ ] Rust unit tests for conversation models
- [ ] Integration tests for API endpoints (using hurl)
  - [ ] Test CRUD operations
  - [ ] Test authentication/authorization
  - [ ] Test pagination and filtering
  - [ ] Test user isolation (can't access other user's conversations)
- [ ] Add test cases to `tests/` directory

### 2.7 E2E Testing
- [ ] Update `dashboard/src/components/features/playground/Playground/Playground.functional.test.tsx`
  - [ ] Mock conversation API endpoints with MSW
  - [ ] Test conversation creation and loading
  - [ ] Test message persistence
  - [ ] Test error states

---

## Phase 3: Polish & Advanced Features (Week 3+)

### 3.1 Search & Filtering
- [ ] Add search input to history sidebar
  - [ ] Full-text search across conversation titles
  - [ ] Search message content (backend support needed)
  - [ ] Filter by model type
  - [ ] Filter by date range
  - [ ] Sort options (newest, oldest, most recent activity)

### 3.2 Export & Sharing
- [ ] Export single conversation
  - [ ] JSON format (full structured data)
  - [ ] Markdown format (readable)
  - [ ] Copy to clipboard option

- [ ] Bulk export
  - [ ] Export all conversations as ZIP
  - [ ] Filter before export

- [ ] Sharing (future)
  - [ ] Generate shareable link
  - [ ] Public/unlisted/private permissions
  - [ ] Expiring links

### 3.3 Organization Features
- [ ] Folders/tags for conversations
  - [ ] Create custom folders
  - [ ] Drag-and-drop to organize
  - [ ] Tag conversations with labels
  - [ ] Filter by folder/tag

- [ ] Favorites/pinning
  - [ ] Star important conversations
  - [ ] Pin to top of list
  - [ ] "Favorites" filter

### 3.4 Analytics
- [ ] Conversation statistics
  - [ ] Total conversations count
  - [ ] Most used models
  - [ ] Average conversation length
  - [ ] Token usage tracking (if available)

### 3.5 Accessibility & UX
- [ ] Keyboard shortcuts
  - [ ] `Cmd+K` - Open conversation search
  - [ ] `Cmd+N` - New conversation
  - [ ] `Cmd+Shift+H` - Toggle history sidebar
  - [ ] Arrow keys to navigate conversation list

- [ ] Screen reader support
  - [ ] ARIA labels for all interactive elements
  - [ ] Announce conversation switches
  - [ ] Keyboard navigation for sidebar

- [ ] Loading states & animations
  - [ ] Skeleton loaders for conversation list
  - [ ] Smooth transitions when switching
  - [ ] Progress indicator for long operations

### 3.6 Settings & Preferences
- [ ] Add to `dashboard/src/components/features/settings/`
  - [ ] Auto-save toggle (on by default)
  - [ ] Storage preference (localStorage vs backend)
  - [ ] Retention policy (keep for 30/60/90 days, forever)
  - [ ] Auto-cleanup old conversations toggle
  - [ ] Export data on demand

---

## Technical Considerations

### Storage Schema (localStorage - Phase 1)
```typescript
// Key: "playground-conversations"
{
  conversations: [
    {
      id: string,              // uuid
      modelAlias: string,
      title: string,
      createdAt: string,       // ISO timestamp
      updatedAt: string,       // ISO timestamp
      messages: Message[]      // existing Message type from Playground.tsx
    }
  ],
  activeConversationId: string | null,
  version: number              // schema version for migrations
}
```

### API Response Schema (Phase 2)
```typescript
// GET /admin/api/v1/conversations
{
  conversations: Conversation[],
  pagination: {
    total: number,
    page: number,
    pageSize: number
  }
}

// GET /admin/api/v1/conversations/:id
{
  id: string,
  userId: string,
  modelAlias: string,
  title: string,
  createdAt: string,
  updatedAt: string,
  messages: ConversationMessage[]
}
```

### Performance Considerations
- Paginate conversation list (20-50 per page)
- Lazy-load messages (only fetch when conversation opened)
- Debounce auto-save (500ms delay)
- Virtual scrolling for long message lists
- Compress localStorage data if approaching limits

### Privacy & Security
- User-scoped conversations (can't access other users' data)
- HTTPS for all API calls
- Sanitize user-generated titles
- Consider encryption for sensitive conversations (future)
- Add admin setting to disable history feature per-user/group

### Migration Path
1. Phase 1 ships → users accumulate localStorage data
2. Phase 2 backend ready → automatic migration prompt
3. Both systems work in parallel for 1-2 weeks
4. Deprecate localStorage as primary storage
5. Keep localStorage as offline cache indefinitely

---

## Success Metrics

### Phase 1
- [ ] Users can create/switch between conversations
- [ ] Conversations persist across page refresh
- [ ] No data loss on model switching
- [ ] UI responsive and intuitive

### Phase 2
- [ ] 95%+ of localStorage data successfully migrated
- [ ] API response times < 200ms (p95)
- [ ] Zero data loss during migration
- [ ] Conversations sync across devices

### Phase 3
- [ ] Search returns results in < 100ms
- [ ] Export works for conversations with 100+ messages
- [ ] Accessibility score 100 (Lighthouse)
- [ ] User feedback positive on organization features

---

## Documentation Tasks
- [ ] Update `CLAUDE.md` with conversation management patterns
- [ ] Document API endpoints in backend README
- [ ] Add JSDoc comments to all new TypeScript functions
- [ ] Create user guide for conversation management
- [ ] Update testing philosophy section with new patterns

---

## Rollout Plan
1. **Dev Testing** - Implement and test locally with synthetic data
2. **Staging** - Deploy to staging environment, test with team
3. **Beta** - Enable for subset of users, gather feedback
4. **GA** - Full rollout with monitoring
5. **Iterate** - Address feedback, add Phase 3 features

---

## Open Questions
- [ ] Should system messages be included in history?
- [ ] How to handle very long conversations (1000+ messages)?
- [ ] Should we support conversation forking (branch from any message)?
- [ ] Image storage strategy for multimodal conversations?
- [ ] Should embeddings/reranking playgrounds also get history?
- [ ] Rate limiting for conversation creation/storage?

---

## Dependencies
- No new npm packages needed for Phase 1
- Phase 2 may need:
  - Backend: `uuid` crate (likely already installed)
  - Frontend: Already have all necessary deps (React Query, etc.)

---

## Estimated Timeline
- **Phase 1**: 4-6 days (localStorage + UI)
- **Phase 2**: 6-8 days (Backend + Migration)
- **Phase 3**: 5-10 days (Polish + Advanced features)
- **Total**: 3-4 weeks for complete implementation

---

## Next Steps
1. Review and approve this plan
2. Start with Phase 1.1 (Core Storage Layer)
3. Iterate quickly with frequent testing
4. Get user feedback after Phase 1 before starting Phase 2
