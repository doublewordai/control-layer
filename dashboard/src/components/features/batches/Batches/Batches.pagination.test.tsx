import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter } from "react-router-dom";
import { Batches } from "./Batches";
import * as hooks from "../../../../api/control-layer/hooks";

// Mock the hooks
vi.mock("../../../../api/control-layer/hooks", () => ({
  useFiles: vi.fn(),
  useBatches: vi.fn(),
  useDeleteFile: vi.fn(),
  useCancelBatch: vi.fn(),
}));

// Mock the modals
vi.mock("../../../modals/CreateFileModal", () => ({
  UploadFileModal: () => null,
}));

vi.mock("../../../modals/CreateBatchModal", () => ({
  CreateBatchModal: () => null,
}));

vi.mock("../../../modals/DownloadFileModal", () => ({
  DownloadFileModal: () => null,
}));

vi.mock("sonner", () => ({
  toast: {
    success: vi.fn(),
    error: vi.fn(),
  },
}));

// Helper to create mock files for pagination
const createMockFiles = (pageNum: number, count: number) => {
  return Array.from({ length: count }, (_, i) => ({
    id: `file-page${pageNum}-${i}`,
    object: "file",
    bytes: 100000 + i * 1000,
    created_at: 1730995200 + i * 86400,
    expires_at: 1765065600 + i * 86400,
    filename: `file_page${pageNum}_${i}.jsonl`,
    purpose: "batch",
  }));
};

// Helper to create mock batches for pagination
const createMockBatches = (pageNum: number, count: number) => {
  return Array.from({ length: count }, (_, i) => ({
    id: `batch-page${pageNum}-${i}`,
    object: "batch",
    endpoint: "/v1/chat/completions",
    errors: null,
    input_file_id: `file-${i}`,
    completion_window: "24h",
    status: "completed" as const,
    output_file_id: `file-output-${i}`,
    error_file_id: null,
    created_at: 1730822400 + i * 86400,
    in_progress_at: 1730824200 + i * 86400,
    expires_at: 1731427200 + i * 86400,
    finalizing_at: 1730865600 + i * 86400,
    completed_at: 1730869200 + i * 86400,
    failed_at: null,
    expired_at: null,
    cancelling_at: null,
    cancelled_at: null,
    request_counts: {
      total: 100,
      completed: 98,
      failed: 2,
    },
    metadata: {},
  }));
};

const defaultProps = {
  onOpenUploadModal: vi.fn(),
  onOpenCreateBatchModal: vi.fn(),
  onOpenDownloadModal: vi.fn(),
  onOpenDeleteDialog: vi.fn(),
  onOpenCancelDialog: vi.fn(),
  onBatchCreatedCallback: vi.fn(),
};

const createWrapper = (initialEntries = ["/"]) => {
  const queryClient = new QueryClient({
    defaultOptions: {
      queries: { retry: false, gcTime: 0, staleTime: 0 },
      mutations: { retry: false },
    },
  });
  return ({ children }: { children: React.ReactNode }) => (
    <MemoryRouter initialEntries={initialEntries}>
      <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
    </MemoryRouter>
  );
};

describe("Batches - Pagination", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  describe("Files Pagination", () => {
    it("should navigate forward to next page and use correct cursor", async () => {
      const user = userEvent.setup();

      // Create mock data: 11 files total (10 per page + 1 to detect hasMore)
      const page1Files = createMockFiles(1, 11);
      const page2Files = createMockFiles(2, 5);

      // Track which cursor was used in the API call
      const useFilesCalls: any[] = [];

      vi.mocked(hooks.useFiles).mockImplementation((params?: any) => {
        useFilesCalls.push(params);

        // Unpaginated query for all files
        if (!params || (!params.purpose && !params.limit)) {
          return {
            data: { data: [] },
            isLoading: false,
            error: null,
            refetch: vi.fn(),
          } as any;
        }

        // Page 1: no cursor
        if (!params.after) {
          return {
            data: { data: page1Files },
            isLoading: false,
            error: null,
            refetch: vi.fn(),
          } as any;
        }

        // Page 2: with cursor
        if (params.after === page1Files[9].id) {
          return {
            data: { data: page2Files },
            isLoading: false,
            error: null,
            refetch: vi.fn(),
          } as any;
        }

        return {
          data: { data: [] },
          isLoading: false,
          error: null,
          refetch: vi.fn(),
        } as any;
      });

      vi.mocked(hooks.useBatches).mockReturnValue({
        data: { data: [] },
        isLoading: false,
        error: null,
        refetch: vi.fn(),
      } as any);

      vi.mocked(hooks.useDeleteFile).mockReturnValue({
        mutateAsync: vi.fn(),
        isPending: false,
      } as any);

      vi.mocked(hooks.useCancelBatch).mockReturnValue({
        mutateAsync: vi.fn(),
        isPending: false,
      } as any);

      render(<Batches {...defaultProps} />, { wrapper: createWrapper() });

      // Wait for initial render
      await waitFor(() => {
        expect(screen.getByText("file_page1_0.jsonl")).toBeInTheDocument();
      });

      // Verify we're on page 1
      expect(screen.getByText(/Page 1/i)).toBeInTheDocument();

      // Click Next button
      const nextButton = screen.getByRole("button", { name: /Next/i });
      await user.click(nextButton);

      // Verify page 2 is shown
      await waitFor(() => {
        expect(screen.getByText(/Page 2/i)).toBeInTheDocument();
      });

      // Verify the correct cursor was used (last item from page 1)
      const page2Calls = useFilesCalls.filter(
        (call) => call?.after === page1Files[9].id
      );
      expect(page2Calls.length).toBeGreaterThan(0);
    });

    it("should navigate backward to previous page using cursor history", async () => {
      const user = userEvent.setup();

      const page1Files = createMockFiles(1, 11);
      const page2Files = createMockFiles(2, 5);

      vi.mocked(hooks.useFiles).mockImplementation((params?: any) => {
        // Unpaginated query
        if (!params || (!params.purpose && !params.limit)) {
          return {
            data: { data: [] },
            isLoading: false,
            error: null,
            refetch: vi.fn(),
          } as any;
        }

        // Page 1: no cursor
        if (!params.after) {
          return {
            data: { data: page1Files },
            isLoading: false,
            error: null,
            refetch: vi.fn(),
          } as any;
        }

        // Page 2: with cursor
        if (params.after === page1Files[9].id) {
          return {
            data: { data: page2Files },
            isLoading: false,
            error: null,
            refetch: vi.fn(),
          } as any;
        }

        return {
          data: { data: [] },
          isLoading: false,
          error: null,
          refetch: vi.fn(),
        } as any;
      });

      vi.mocked(hooks.useBatches).mockReturnValue({
        data: { data: [] },
        isLoading: false,
        error: null,
        refetch: vi.fn(),
      } as any);

      vi.mocked(hooks.useDeleteFile).mockReturnValue({
        mutateAsync: vi.fn(),
        isPending: false,
      } as any);

      vi.mocked(hooks.useCancelBatch).mockReturnValue({
        mutateAsync: vi.fn(),
        isPending: false,
      } as any);

      render(<Batches {...defaultProps} />, { wrapper: createWrapper() });

      // Wait for page 1
      await waitFor(() => {
        expect(screen.getByText("file_page1_0.jsonl")).toBeInTheDocument();
      });

      // Navigate to page 2
      const nextButton = screen.getByRole("button", { name: /Next/i });
      await user.click(nextButton);

      await waitFor(() => {
        expect(screen.getByText(/Page 2/i)).toBeInTheDocument();
      });

      // Now click Previous - should go back to page 1 using cursor history
      const prevButton = screen.getByRole("button", { name: /Previous/i });
      await user.click(prevButton);

      // Should be back on page 1
      await waitFor(() => {
        expect(screen.getByText(/Page 1/i)).toBeInTheDocument();
      });

      // The previous button should now be disabled (we're on page 0)
      expect(prevButton).toBeDisabled();
    });

    it("should show First button only when on page 2 or higher", async () => {
      const user = userEvent.setup();

      const page1Files = createMockFiles(1, 11);
      const page2Files = createMockFiles(2, 11);
      const page3Files = createMockFiles(3, 5);

      vi.mocked(hooks.useFiles).mockImplementation((params?: any) => {
        if (!params || (!params.purpose && !params.limit)) {
          return {
            data: { data: [] },
            isLoading: false,
            error: null,
            refetch: vi.fn(),
          } as any;
        }

        if (!params.after) {
          return {
            data: { data: page1Files },
            isLoading: false,
            error: null,
            refetch: vi.fn(),
          } as any;
        }

        if (params.after === page1Files[9].id) {
          return {
            data: { data: page2Files },
            isLoading: false,
            error: null,
            refetch: vi.fn(),
          } as any;
        }

        if (params.after === page2Files[9].id) {
          return {
            data: { data: page3Files },
            isLoading: false,
            error: null,
            refetch: vi.fn(),
          } as any;
        }

        return {
          data: { data: [] },
          isLoading: false,
          error: null,
          refetch: vi.fn(),
        } as any;
      });

      vi.mocked(hooks.useBatches).mockReturnValue({
        data: { data: [] },
        isLoading: false,
        error: null,
        refetch: vi.fn(),
      } as any);

      vi.mocked(hooks.useDeleteFile).mockReturnValue({
        mutateAsync: vi.fn(),
        isPending: false,
      } as any);

      vi.mocked(hooks.useCancelBatch).mockReturnValue({
        mutateAsync: vi.fn(),
        isPending: false,
      } as any);

      render(<Batches {...defaultProps} />, { wrapper: createWrapper() });

      await waitFor(() => {
        expect(screen.getByText("file_page1_0.jsonl")).toBeInTheDocument();
      });

      // Page 1: No First button
      expect(screen.queryByRole("button", { name: /First/i })).not.toBeInTheDocument();

      // Navigate to page 2
      const nextButton = screen.getByRole("button", { name: /Next/i });
      await user.click(nextButton);

      await waitFor(() => {
        expect(screen.getByText(/Page 2/i)).toBeInTheDocument();
      });

      // Page 2: Still no First button (only shows on page 3+)
      expect(screen.queryByRole("button", { name: /First/i })).not.toBeInTheDocument();

      // Navigate to page 3
      await user.click(nextButton);

      await waitFor(() => {
        expect(screen.getByText(/Page 3/i)).toBeInTheDocument();
      });

      // Page 3: First button should appear
      expect(screen.getByRole("button", { name: /First/i })).toBeInTheDocument();
    });

    it("should jump to first page and clear history when First button clicked", async () => {
      const user = userEvent.setup();

      const page1Files = createMockFiles(1, 11);
      const page2Files = createMockFiles(2, 11);
      const page3Files = createMockFiles(3, 5);

      let wasResetToPageOne = false;

      vi.mocked(hooks.useFiles).mockImplementation((params?: any) => {
        if (!params || (!params.purpose && !params.limit)) {
          return {
            data: { data: [] },
            isLoading: false,
            error: null,
            refetch: vi.fn(),
          } as any;
        }

        // Track if we reset to page 1 after being on a higher page
        if (!params.after && wasResetToPageOne) {
          return {
            data: { data: page1Files },
            isLoading: false,
            error: null,
            refetch: vi.fn(),
          } as any;
        }

        if (!params.after) {
          return {
            data: { data: page1Files },
            isLoading: false,
            error: null,
            refetch: vi.fn(),
          } as any;
        }

        if (params.after === page1Files[9].id) {
          return {
            data: { data: page2Files },
            isLoading: false,
            error: null,
            refetch: vi.fn(),
          } as any;
        }

        if (params.after === page2Files[9].id) {
          wasResetToPageOne = true;
          return {
            data: { data: page3Files },
            isLoading: false,
            error: null,
            refetch: vi.fn(),
          } as any;
        }

        return {
          data: { data: [] },
          isLoading: false,
          error: null,
          refetch: vi.fn(),
        } as any;
      });

      vi.mocked(hooks.useBatches).mockReturnValue({
        data: { data: [] },
        isLoading: false,
        error: null,
        refetch: vi.fn(),
      } as any);

      vi.mocked(hooks.useDeleteFile).mockReturnValue({
        mutateAsync: vi.fn(),
        isPending: false,
      } as any);

      vi.mocked(hooks.useCancelBatch).mockReturnValue({
        mutateAsync: vi.fn(),
        isPending: false,
      } as any);

      render(<Batches {...defaultProps} />, { wrapper: createWrapper() });

      await waitFor(() => {
        expect(screen.getByText("file_page1_0.jsonl")).toBeInTheDocument();
      });

      // Navigate to page 2, then page 3
      const nextButton = screen.getByRole("button", { name: /Next/i });
      await user.click(nextButton);
      await waitFor(() => {
        expect(screen.getByText(/Page 2/i)).toBeInTheDocument();
      });

      await user.click(nextButton);
      await waitFor(() => {
        expect(screen.getByText(/Page 3/i)).toBeInTheDocument();
      });

      // Click First button
      const firstButton = screen.getByRole("button", { name: /First/i });
      await user.click(firstButton);

      // Should be back on page 1
      await waitFor(() => {
        expect(screen.getByText(/Page 1/i)).toBeInTheDocument();
      });

      // First button should no longer be visible
      expect(screen.queryByRole("button", { name: /First/i })).not.toBeInTheDocument();
    });

    it("should clear cursor history when page size changes", async () => {
      const user = userEvent.setup();

      const smallPageFiles = createMockFiles(1, 11);
      const largePageFiles = createMockFiles(1, 26); // 25 per page + 1

      vi.mocked(hooks.useFiles).mockImplementation((params?: any) => {
        if (!params || (!params.purpose && !params.limit)) {
          return {
            data: { data: [] },
            isLoading: false,
            error: null,
            refetch: vi.fn(),
          } as any;
        }

        // Return different data based on page size
        if (params.limit === 11) {
          return {
            data: { data: smallPageFiles },
            isLoading: false,
            error: null,
            refetch: vi.fn(),
          } as any;
        }

        if (params.limit === 26) {
          return {
            data: { data: largePageFiles },
            isLoading: false,
            error: null,
            refetch: vi.fn(),
          } as any;
        }

        return {
          data: { data: [] },
          isLoading: false,
          error: null,
          refetch: vi.fn(),
        } as any;
      });

      vi.mocked(hooks.useBatches).mockReturnValue({
        data: { data: [] },
        isLoading: false,
        error: null,
        refetch: vi.fn(),
      } as any);

      vi.mocked(hooks.useDeleteFile).mockReturnValue({
        mutateAsync: vi.fn(),
        isPending: false,
      } as any);

      vi.mocked(hooks.useCancelBatch).mockReturnValue({
        mutateAsync: vi.fn(),
        isPending: false,
      } as any);

      render(<Batches {...defaultProps} />, { wrapper: createWrapper() });

      await waitFor(() => {
        expect(screen.getByText("file_page1_0.jsonl")).toBeInTheDocument();
      });

      // Navigate to page 2
      const nextButton = screen.getByRole("button", { name: /Next/i });
      await user.click(nextButton);

      await waitFor(() => {
        expect(screen.getByText(/Page 2/i)).toBeInTheDocument();
      });

      // Change page size by clicking the combobox trigger
      const pageSizeSelect = screen.getByRole("combobox");
      await user.click(pageSizeSelect);

      // Wait for the dropdown to open and find the option by text
      // Radix UI Select uses data-radix-collection-item for options
      await waitFor(() => {
        expect(screen.getByText("25")).toBeInTheDocument();
      });

      // Click the option
      const option25 = screen.getByText("25");
      await user.click(option25);

      // Should reset to page 1 after changing page size
      await waitFor(() => {
        expect(screen.getByText(/Page 1/i)).toBeInTheDocument();
      });

      // Previous button should be disabled (back at page 1)
      expect(screen.getByRole("button", { name: /Previous/i })).toBeDisabled();
    });
  });

  describe("Batches Pagination", () => {
    it("should navigate forward through batch pages", async () => {
      const user = userEvent.setup();

      const page1Batches = createMockBatches(1, 11);
      const page2Batches = createMockBatches(2, 5);

      vi.mocked(hooks.useFiles).mockReturnValue({
        data: { data: [] },
        isLoading: false,
        error: null,
        refetch: vi.fn(),
      } as any);

      vi.mocked(hooks.useBatches).mockImplementation((params?: any) => {
        if (!params?.after) {
          return {
            data: { data: page1Batches },
            isLoading: false,
            error: null,
            refetch: vi.fn(),
          } as any;
        }

        if (params.after === page1Batches[9].id) {
          return {
            data: { data: page2Batches },
            isLoading: false,
            error: null,
            refetch: vi.fn(),
          } as any;
        }

        return {
          data: { data: [] },
          isLoading: false,
          error: null,
          refetch: vi.fn(),
        } as any;
      });

      vi.mocked(hooks.useDeleteFile).mockReturnValue({
        mutateAsync: vi.fn(),
        isPending: false,
      } as any);

      vi.mocked(hooks.useCancelBatch).mockReturnValue({
        mutateAsync: vi.fn(),
        isPending: false,
      } as any);

      render(<Batches {...defaultProps} />, { wrapper: createWrapper() });

      // Switch to batches tab
      await user.click(screen.getByRole("tab", { name: /batches/i }));

      await waitFor(() => {
        expect(screen.getByText(/Page 1/i)).toBeInTheDocument();
      });

      // Navigate to page 2
      const nextButton = screen.getByRole("button", { name: /Next/i });
      await user.click(nextButton);

      await waitFor(() => {
        expect(screen.getByText(/Page 2/i)).toBeInTheDocument();
      });
    });

    it("should navigate backward through batch pages using cursor history", async () => {
      const user = userEvent.setup();

      const page1Batches = createMockBatches(1, 11);
      const page2Batches = createMockBatches(2, 5);

      vi.mocked(hooks.useFiles).mockReturnValue({
        data: { data: [] },
        isLoading: false,
        error: null,
        refetch: vi.fn(),
      } as any);

      vi.mocked(hooks.useBatches).mockImplementation((params?: any) => {
        if (!params?.after) {
          return {
            data: { data: page1Batches },
            isLoading: false,
            error: null,
            refetch: vi.fn(),
          } as any;
        }

        if (params.after === page1Batches[9].id) {
          return {
            data: { data: page2Batches },
            isLoading: false,
            error: null,
            refetch: vi.fn(),
          } as any;
        }

        return {
          data: { data: [] },
          isLoading: false,
          error: null,
          refetch: vi.fn(),
        } as any;
      });

      vi.mocked(hooks.useDeleteFile).mockReturnValue({
        mutateAsync: vi.fn(),
        isPending: false,
      } as any);

      vi.mocked(hooks.useCancelBatch).mockReturnValue({
        mutateAsync: vi.fn(),
        isPending: false,
      } as any);

      render(<Batches {...defaultProps} />, { wrapper: createWrapper() });

      // Switch to batches tab
      await user.click(screen.getByRole("tab", { name: /batches/i }));

      await waitFor(() => {
        expect(screen.getByText(/Page 1/i)).toBeInTheDocument();
      });

      // Navigate to page 2
      const nextButton = screen.getByRole("button", { name: /Next/i });
      await user.click(nextButton);

      await waitFor(() => {
        expect(screen.getByText(/Page 2/i)).toBeInTheDocument();
      });

      // Navigate back to page 1
      const prevButton = screen.getByRole("button", { name: /Previous/i });
      await user.click(prevButton);

      await waitFor(() => {
        expect(screen.getByText(/Page 1/i)).toBeInTheDocument();
      });

      // Previous button should be disabled on page 1
      expect(prevButton).toBeDisabled();
    });
  });
});
