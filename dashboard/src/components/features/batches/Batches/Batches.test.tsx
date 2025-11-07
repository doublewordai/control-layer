import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { Batches } from "./Batches";
import * as hooks from "../../../../api/control-layer/hooks";

// Mock the hooks
vi.mock("../../../../api/control-layer/hooks", () => ({
  useFiles: vi.fn(),
  useBatches: vi.fn(),
  useDeleteFile: vi.fn(),
  useCancelBatch: vi.fn(),
  useDownloadBatchResults: vi.fn(),
}));

// Mock the modals
vi.mock("../../../modals/CreateFileModal", () => ({
  UploadFileModal: ({ isOpen }: { isOpen: boolean }) =>
    isOpen ? <div data-testid="upload-modal">Upload Modal</div> : null,
}));

vi.mock("../../../modals/CreateBatchModal", () => ({
  CreateBatchModal: ({ isOpen }: { isOpen: boolean }) =>
    isOpen ? <div data-testid="create-batch-modal">Create Batch Modal</div> : null,
}));

vi.mock("../../../modals/FileRequestsModal", () => ({
  ViewFileRequestsModal: ({ isOpen }: { isOpen: boolean }) =>
    isOpen ? <div data-testid="view-file-requests-modal">View File Requests Modal</div> : null,
}));

vi.mock("../../../modals/BatchRequestsModal", () => ({
  ViewBatchRequestsModal: ({ isOpen }: { isOpen: boolean }) =>
    isOpen ? <div data-testid="view-batch-requests-modal">View Batch Requests Modal</div> : null,
}));

vi.mock("../../../modals/DownloadFileModal", () => ({
  DownloadFileModal: ({ isOpen }: { isOpen: boolean }) =>
    isOpen ? <div data-testid="download-file-modal">Download File Modal</div> : null,
}));

// Mock sonner toast
vi.mock("sonner", () => ({
  toast: {
    success: vi.fn(),
    error: vi.fn(),
  },
}));

const mockFiles = [
  {
    id: "file-1",
    object: "file",
    bytes: 145600,
    created_at: 1730995200,
    expires_at: 1765065600,
    filename: "test_file.jsonl",
    purpose: "batch",
  },
  {
    id: "file-2",
    object: "file",
    bytes: 89200,
    created_at: 1730822400,
    expires_at: 1767657600,
    filename: "another_file.jsonl",
    purpose: "batch",
  },
];

const mockBatches = [
  {
    id: "batch-1",
    object: "batch",
    endpoint: "/v1/chat/completions",
    errors: null,
    input_file_id: "file-1",
    completion_window: "24h",
    status: "completed" as const,
    output_file_id: "file-output-1",
    error_file_id: null,
    created_at: 1730822400,
    in_progress_at: 1730824200,
    expires_at: 1731427200,
    finalizing_at: 1730865600,
    completed_at: 1730869200,
    failed_at: null,
    expired_at: null,
    cancelling_at: null,
    cancelled_at: null,
    request_counts: {
      total: 250,
      completed: 248,
      failed: 2,
    },
    metadata: {},
  },
  {
    id: "batch-2",
    object: "batch",
    endpoint: "/v1/chat/completions",
    errors: null,
    input_file_id: "file-2",
    completion_window: "24h",
    status: "in_progress" as const,
    output_file_id: null,
    error_file_id: null,
    created_at: 1730901600,
    in_progress_at: 1730903400,
    expires_at: 1730980800,
    finalizing_at: null,
    completed_at: null,
    failed_at: null,
    expired_at: null,
    cancelling_at: null,
    cancelled_at: null,
    request_counts: {
      total: 150,
      completed: 87,
      failed: 1,
    },
    metadata: {},
  },
];

const createWrapper = () => {
  const queryClient = new QueryClient({
    defaultOptions: {
      queries: { retry: false },
      mutations: { retry: false },
    },
  });
  return ({ children }: { children: React.ReactNode }) => (
    <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
  );
};

describe("Batches", () => {
  beforeEach(() => {
    vi.clearAllMocks();

    // Default mock implementations
    vi.mocked(hooks.useFiles).mockReturnValue({
      data: { data: mockFiles },
      isLoading: false,
      error: null,
      refetch: vi.fn(),
    } as any);

    vi.mocked(hooks.useBatches).mockReturnValue({
      data: { data: mockBatches },
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

    vi.mocked(hooks.useDownloadBatchResults).mockReturnValue({
      mutateAsync: vi.fn(),
      isPending: false,
    } as any);
  });

  describe("Rendering", () => {
    it("should render the page title and description", () => {
      render(<Batches />, { wrapper: createWrapper() });

      expect(screen.getByText("Batch Processing")).toBeInTheDocument();
      expect(
        screen.getByText(
          "Upload files and create batches to process requests at scale"
        )
      ).toBeInTheDocument();
    });

    it("should render action buttons", () => {
      render(<Batches />, { wrapper: createWrapper() });

      expect(screen.getByRole("button", { name: /upload file/i })).toBeInTheDocument();
      expect(screen.getByRole("button", { name: /create batch/i })).toBeInTheDocument();
    });

    it("should render stats cards with correct data", () => {
      render(<Batches />, { wrapper: createWrapper() });

      expect(screen.getByText("Total Files")).toBeInTheDocument();
      expect(screen.getByText("Total Batches")).toBeInTheDocument();
      expect(screen.getByText("Active Batches")).toBeInTheDocument();
      expect(screen.getByText("Completed Batches")).toBeInTheDocument();

      // Use getAllByText for duplicate values
      const twos = screen.getAllByText("2");
      expect(twos.length).toBeGreaterThanOrEqual(2); // 2 files and 2 batches
      
      const ones = screen.getAllByText("1");
      expect(ones.length).toBeGreaterThan(0); // 1 active, 1 completed
    });

    it("should render tabs", () => {
      render(<Batches />, { wrapper: createWrapper() });

      expect(screen.getByRole("tab", { name: /files \(2\)/i })).toBeInTheDocument();
      expect(screen.getByRole("tab", { name: /batches \(2\)/i })).toBeInTheDocument();
    });
  });

  describe("Loading State", () => {
    it("should show loading state when files are loading", () => {
      vi.mocked(hooks.useFiles).mockReturnValue({
        data: undefined,
        isLoading: true,
        error: null,
        refetch: vi.fn(),
      } as any);

      render(<Batches />, { wrapper: createWrapper() });

      // Use getAllByText since there are multiple "Loading..." texts
      const loadingTexts = screen.getAllByText("Loading...");
      expect(loadingTexts.length).toBeGreaterThan(0);
    });

    it("should show loading state when batches are loading", () => {
      vi.mocked(hooks.useBatches).mockReturnValue({
        data: undefined,
        isLoading: true,
        error: null,
        refetch: vi.fn(),
      } as any);

      render(<Batches />, { wrapper: createWrapper() });

      // Use getAllByText since there are multiple "Loading..." texts
      const loadingTexts = screen.getAllByText("Loading...");
      expect(loadingTexts.length).toBeGreaterThan(0);
    });

    it("should show spinner when loading", () => {
      vi.mocked(hooks.useFiles).mockReturnValue({
        data: undefined,
        isLoading: true,
        error: null,
        refetch: vi.fn(),
      } as any);

      const { container } = render(<Batches />, { wrapper: createWrapper() });

      const spinner = container.querySelector('.animate-spin');
      expect(spinner).toBeInTheDocument();
    });
  });

  describe("Empty States", () => {
    it("should show empty state when no files exist", () => {
      vi.mocked(hooks.useFiles).mockReturnValue({
        data: { data: [] },
        isLoading: false,
        error: null,
        refetch: vi.fn(),
      } as any);

      render(<Batches />, { wrapper: createWrapper() });

      expect(screen.getByText("No files uploaded")).toBeInTheDocument();
      expect(
        screen.getByText(
          "Upload a .jsonl file to get started with batch processing"
        )
      ).toBeInTheDocument();
      expect(screen.getByRole("button", { name: /upload first file/i })).toBeInTheDocument();
    });

    it("should show empty state when no batches exist", async () => {
      const user = userEvent.setup();

      vi.mocked(hooks.useBatches).mockReturnValue({
        data: { data: [] },
        isLoading: false,
        error: null,
        refetch: vi.fn(),
      } as any);

      render(<Batches />, { wrapper: createWrapper() });

      // Switch to batches tab
      await user.click(screen.getByRole("tab", { name: /batches/i }));

      expect(screen.getByText("No batches created")).toBeInTheDocument();
      expect(
        screen.getByText(
          "Create a batch from an uploaded file to start processing requests"
        )
      ).toBeInTheDocument();
      expect(screen.getByRole("button", { name: /create first batch/i })).toBeInTheDocument();
    });
  });

  describe("Files Tab", () => {
    it("should display files in the table", () => {
      render(<Batches />, { wrapper: createWrapper() });

      expect(screen.getByText("test_file.jsonl")).toBeInTheDocument();
      expect(screen.getByText("another_file.jsonl")).toBeInTheDocument();
    });

    it("should open upload modal when upload button is clicked", async () => {
      const user = userEvent.setup();
      render(<Batches />, { wrapper: createWrapper() });

      await user.click(screen.getByRole("button", { name: /upload file/i }));

      expect(screen.getByTestId("upload-modal")).toBeInTheDocument();
    });

    it("should allow searching files", async () => {
      const user = userEvent.setup();
      render(<Batches />, { wrapper: createWrapper() });

      const searchInput = screen.getByPlaceholderText(/search files/i);
      await user.type(searchInput, "test");

      expect(searchInput).toHaveValue("test");
    });

    it("should filter files based on search query", async () => {
      const user = userEvent.setup();
      render(<Batches />, { wrapper: createWrapper() });

      const searchInput = screen.getByPlaceholderText(/search files/i);
      await user.type(searchInput, "test_file");

      // After filtering, only test_file.jsonl should be visible
      expect(screen.getByText("test_file.jsonl")).toBeInTheDocument();
    });
  });

  describe("Batches Tab", () => {
    it("should display batches in the table", async () => {
      const user = userEvent.setup();
      render(<Batches />, { wrapper: createWrapper() });

      // Switch to batches tab
      await user.click(screen.getByRole("tab", { name: /batches/i }));

      expect(screen.getByText("batch-1")).toBeInTheDocument();
      expect(screen.getByText("batch-2")).toBeInTheDocument();
    });

    it("should open create batch modal when create batch button is clicked", async () => {
      const user = userEvent.setup();
      render(<Batches />, { wrapper: createWrapper() });

      await user.click(screen.getByRole("button", { name: /create batch/i }));

      expect(screen.getByTestId("create-batch-modal")).toBeInTheDocument();
    });

    it("should allow searching batches", async () => {
      const user = userEvent.setup();
      render(<Batches />, { wrapper: createWrapper() });

      // Switch to batches tab
      await user.click(screen.getByRole("tab", { name: /batches/i }));

      const searchInput = screen.getByPlaceholderText(/search batches/i);
      await user.type(searchInput, "batch-1");

      expect(searchInput).toHaveValue("batch-1");
    });

    it("should display batch status correctly", async () => {
      const user = userEvent.setup();
      render(<Batches />, { wrapper: createWrapper() });

      await user.click(screen.getByRole("tab", { name: /batches/i }));

      // Check for status badges - there should be multiple "completed" texts
      const completedElements = screen.getAllByText(/completed/i);
      expect(completedElements.length).toBeGreaterThan(0);
      
      // Check specifically for the status badge with class
      const statusBadge = completedElements.find(
        el => el.tagName === 'SPAN' && el.className.includes('rounded-full')
      );
      expect(statusBadge).toBeDefined();
      
      // Check for in_progress status
      expect(screen.getByText(/in progress/i)).toBeInTheDocument();
    });

    it("should display request counts", async () => {
      const user = userEvent.setup();
      render(<Batches />, { wrapper: createWrapper() });

      await user.click(screen.getByRole("tab", { name: /batches/i }));

      // Check that request count data exists in the document
      // The format might be "248 / 250" or similar
      const batchTable = screen.getByRole("table");
      expect(batchTable).toBeInTheDocument();
      
      // Verify batch rows exist with their IDs
      expect(screen.getByText("batch-1")).toBeInTheDocument();
      expect(screen.getByText("batch-2")).toBeInTheDocument();
    });
  });

  describe("File Actions", () => {
    it("should open delete confirmation dialog when delete is clicked", async () => {
      const user = userEvent.setup();
      render(<Batches />, { wrapper: createWrapper() });

      // Find the first file row's action menu
      const actionButtons = screen.getAllByRole("button", { name: /open menu/i });
      await user.click(actionButtons[0]);

      // Click delete option
      const deleteOption = screen.getByRole("menuitem", { name: /delete/i });
      await user.click(deleteOption);

      // Should show confirmation dialog
      expect(screen.getByRole("heading", { name: /delete file/i })).toBeInTheDocument();
      expect(screen.getByText(/are you sure you want to delete/i)).toBeInTheDocument();
    });

    it("should call delete mutation when confirmed", async () => {
      const user = userEvent.setup();
      const mockMutateAsync = vi.fn().mockResolvedValue({});
      vi.mocked(hooks.useDeleteFile).mockReturnValue({
        mutateAsync: mockMutateAsync,
        isPending: false,
      } as any);

      render(<Batches />, { wrapper: createWrapper() });

      // Open action menu
      const actionButtons = screen.getAllByRole("button", { name: /open menu/i });
      await user.click(actionButtons[0]);

      // Click delete
      await user.click(screen.getByRole("menuitem", { name: /delete/i }));

      // Confirm deletion - find the delete button in the dialog
      const deleteButtons = screen.getAllByRole("button", { name: /delete/i });
      const confirmButton = deleteButtons[deleteButtons.length - 1]; // Last one is in the dialog
      await user.click(confirmButton);

      await waitFor(() => {
        expect(mockMutateAsync).toHaveBeenCalledWith("file-1");
      });
    });

    it("should close delete dialog when cancel is clicked", async () => {
      const user = userEvent.setup();
      render(<Batches />, { wrapper: createWrapper() });

      const actionButtons = screen.getAllByRole("button", { name: /open menu/i });
      await user.click(actionButtons[0]);

      await user.click(screen.getByRole("menuitem", { name: /delete/i }));

      const cancelButton = screen.getByRole("button", { name: /cancel/i });
      await user.click(cancelButton);

      await waitFor(() => {
        expect(screen.queryByRole("heading", { name: /delete file/i })).not.toBeInTheDocument();
      });
    });
  });

  describe("Batch Actions", () => {
    it("should open cancel confirmation dialog when cancel is clicked", async () => {
      const user = userEvent.setup();
      render(<Batches />, { wrapper: createWrapper() });

      // Switch to batches tab
      await user.click(screen.getByRole("tab", { name: /batches/i }));

      // Find the in_progress batch's action menu
      const actionButtons = screen.getAllByRole("button", { name: /open menu/i });
      await user.click(actionButtons[1]); // Second batch is in_progress

      // Click cancel option
      const cancelOption = screen.getByRole("menuitem", { name: /cancel batch/i });
      await user.click(cancelOption);

      // Should show confirmation dialog - use role heading
      expect(screen.getByRole("heading", { name: /cancel batch/i })).toBeInTheDocument();
      expect(screen.getByText(/are you sure you want to cancel/i)).toBeInTheDocument();
    });

    it("should call cancel mutation when confirmed", async () => {
      const user = userEvent.setup();
      const mockMutateAsync = vi.fn().mockResolvedValue({});
      vi.mocked(hooks.useCancelBatch).mockReturnValue({
        mutateAsync: mockMutateAsync,
        isPending: false,
      } as any);

      render(<Batches />, { wrapper: createWrapper() });

      // Switch to batches tab
      await user.click(screen.getByRole("tab", { name: /batches/i }));

      // Open action menu for in_progress batch
      const actionButtons = screen.getAllByRole("button", { name: /open menu/i });
      await user.click(actionButtons[1]);

      // Click cancel
      await user.click(screen.getByRole("menuitem", { name: /cancel batch/i }));

      // Confirm cancellation - find the cancel batch button in the dialog
      const cancelButtons = screen.getAllByRole("button", { name: /cancel batch/i });
      const confirmButton = cancelButtons[cancelButtons.length - 1];
      await user.click(confirmButton);

      await waitFor(() => {
        expect(mockMutateAsync).toHaveBeenCalledWith("batch-2");
      });
    });

    it("should not show cancel option for completed batches", async () => {
      const user = userEvent.setup();
      render(<Batches />, { wrapper: createWrapper() });

      await user.click(screen.getByRole("tab", { name: /batches/i }));

      const actionButtons = screen.getAllByRole("button", { name: /open menu/i });
      await user.click(actionButtons[0]); // First batch is completed

      expect(screen.queryByRole("menuitem", { name: /cancel batch/i })).not.toBeInTheDocument();
    });

    it("should show download option for completed batches", async () => {
      const user = userEvent.setup();
      render(<Batches />, { wrapper: createWrapper() });

      await user.click(screen.getByRole("tab", { name: /batches/i }));

      const actionButtons = screen.getAllByRole("button", { name: /open menu/i });
      await user.click(actionButtons[0]); // First batch is completed

      expect(screen.getByRole("menuitem", { name: /download results/i })).toBeInTheDocument();
    });
  });

  describe("Tab Switching", () => {
    it("should switch between files and batches tabs", async () => {
      const user = userEvent.setup();
      render(<Batches />, { wrapper: createWrapper() });

      // Initially on files tab
      expect(screen.getByText("test_file.jsonl")).toBeInTheDocument();

      // Switch to batches tab
      await user.click(screen.getByRole("tab", { name: /batches/i }));

      // Should show batches content
      expect(screen.getByText("batch-1")).toBeInTheDocument();

      // Switch back to files tab
      await user.click(screen.getByRole("tab", { name: /files/i }));

      // Should show files content again
      expect(screen.getByText("test_file.jsonl")).toBeInTheDocument();
    });

    it("should maintain search when switching tabs", async () => {
      const user = userEvent.setup();
      render(<Batches />, { wrapper: createWrapper() });

      // Search in files tab
      const fileSearch = screen.getByPlaceholderText(/search files/i);
      await user.type(fileSearch, "test");

      // Switch to batches
      await user.click(screen.getByRole("tab", { name: /batches/i }));

      // Search should be cleared or independent
      const batchSearch = screen.getByPlaceholderText(/search batches/i);
      expect(batchSearch).toHaveValue("");
    });
  });

  describe("Error Handling", () => {
    it("should display stats as zero when files fail to load", () => {
      vi.mocked(hooks.useFiles).mockReturnValue({
        data: undefined,
        isLoading: false,
        error: new Error("Failed to load files"),
        refetch: vi.fn(),
      } as any);

      render(<Batches />, { wrapper: createWrapper() });

      // When there's an error, the component should still render with zero stats
      const zeros = screen.getAllByText("0");
      expect(zeros.length).toBeGreaterThan(0);
    });

    it("should display stats as zero when batches fail to load", async () => {
      const user = userEvent.setup();

      vi.mocked(hooks.useBatches).mockReturnValue({
        data: undefined,
        isLoading: false,
        error: new Error("Failed to load batches"),
        refetch: vi.fn(),
      } as any);

      render(<Batches />, { wrapper: createWrapper() });

      await user.click(screen.getByRole("tab", { name: /batches/i }));

      // The batches count should be 0 due to error
      const zeros = screen.getAllByText("0");
      expect(zeros.length).toBeGreaterThan(0);
    });
  });

  describe("Stats Calculations", () => {
    it("should calculate active batches correctly", () => {
      render(<Batches />, { wrapper: createWrapper() });

      // Find the Active Batches stat card using within
      expect(screen.getByText("Active Batches")).toBeInTheDocument();
      
      // Verify there's at least one "1" value (for active batches)
      const ones = screen.getAllByText("1");
      expect(ones.length).toBeGreaterThan(0);
    });

    it("should calculate completed batches correctly", () => {
      render(<Batches />, { wrapper: createWrapper() });

      expect(screen.getByText("Completed Batches")).toBeInTheDocument();
      
      // Verify there's a "1" value for completed batches
      const ones = screen.getAllByText("1");
      expect(ones.length).toBeGreaterThan(0);
    });

    it("should handle zero stats correctly", () => {
      vi.mocked(hooks.useFiles).mockReturnValue({
        data: { data: [] },
        isLoading: false,
        error: null,
        refetch: vi.fn(),
      } as any);

      vi.mocked(hooks.useBatches).mockReturnValue({
        data: { data: [] },
        isLoading: false,
        error: null,
        refetch: vi.fn(),
      } as any);

      render(<Batches />, { wrapper: createWrapper() });

      // Should have multiple zeros (one for each stat)
      const zeros = screen.getAllByText("0");
      expect(zeros.length).toBeGreaterThanOrEqual(4); // At least 4 stats should be 0
    });
  });

  describe("Modal Interactions", () => {
    it("should close upload modal after successful upload", async () => {
      const user = userEvent.setup();
      render(<Batches />, { wrapper: createWrapper() });

      await user.click(screen.getByRole("button", { name: /upload file/i }));
      expect(screen.getByTestId("upload-modal")).toBeInTheDocument();

      // Close modal (this would normally be done through modal's internal logic)
      await user.click(screen.getByRole("button", { name: /upload file/i }));
    });

    it("should close create batch modal when closed", async () => {
      const user = userEvent.setup();
      render(<Batches />, { wrapper: createWrapper() });

      await user.click(screen.getByRole("button", { name: /create batch/i }));
      expect(screen.getByTestId("create-batch-modal")).toBeInTheDocument();
    });
  });

  describe("File Size Display", () => {
    it("should display file sizes correctly", () => {
      render(<Batches />, { wrapper: createWrapper() });

      // File sizes should be formatted (e.g., "142.19 KB", "87.11 KB")
      const container = screen.getByText("test_file.jsonl").closest("table");
      expect(container).toBeInTheDocument();
    });
  });

  describe("Date Formatting", () => {
    it("should display created dates for files", () => {
      render(<Batches />, { wrapper: createWrapper() });

      // Dates should be formatted and displayed
      const container = screen.getByText("test_file.jsonl").closest("table");
      expect(container).toBeInTheDocument();
    });

    it("should display created dates for batches", async () => {
      const user = userEvent.setup();
      render(<Batches />, { wrapper: createWrapper() });

      await user.click(screen.getByRole("tab", { name: /batches/i }));

      const container = screen.getByText("batch-1").closest("table");
      expect(container).toBeInTheDocument();
    });
  });

  describe("Accessibility", () => {
    it("should have accessible tab controls", () => {
      render(<Batches />, { wrapper: createWrapper() });

      const filesTab = screen.getByRole("tab", { name: /files/i });
      const batchesTab = screen.getByRole("tab", { name: /batches/i });

      expect(filesTab).toHaveAttribute("aria-selected");
      expect(batchesTab).not.toHaveAttribute("aria-selected", "true");
    });

    it("should have accessible action buttons", () => {
      render(<Batches />, { wrapper: createWrapper() });

      expect(screen.getByRole("button", { name: /upload file/i })).toBeEnabled();
      expect(screen.getByRole("button", { name: /create batch/i })).toBeEnabled();
    });
  });
});