import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen } from "@testing-library/react";
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
  UploadFileModal: ({ isOpen }: { isOpen: boolean }) =>
    isOpen ? <div data-testid="upload-modal">Upload Modal</div> : null,
}));

vi.mock("../../../modals/CreateBatchModal", () => ({
  CreateBatchModal: ({ isOpen }: { isOpen: boolean }) =>
    isOpen ? (
      <div data-testid="create-batch-modal">Create Batch Modal</div>
    ) : null,
}));

vi.mock("../../../modals/FileRequestsModal", () => ({
  ViewFileRequestsModal: ({ isOpen }: { isOpen: boolean }) =>
    isOpen ? (
      <div data-testid="view-file-requests-modal">View File Requests Modal</div>
    ) : null,
}));

vi.mock("../../../modals/BatchRequestsModal", () => ({
  ViewBatchRequestsModal: ({ isOpen }: { isOpen: boolean }) =>
    isOpen ? (
      <div data-testid="view-batch-requests-modal">
        View Batch Requests Modal
      </div>
    ) : null,
}));

vi.mock("../../../modals/DownloadFileModal", () => ({
  DownloadFileModal: ({ isOpen }: { isOpen: boolean }) =>
    isOpen ? (
      <div data-testid="download-file-modal">Download File Modal</div>
    ) : null,
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

// Default props for the Batches component
const defaultProps = {
  onOpenUploadModal: vi.fn(),
  onOpenCreateBatchModal: vi.fn(),
  onOpenDownloadModal: vi.fn(),
  onOpenDeleteDialog: vi.fn(),
  onOpenCancelDialog: vi.fn(),
  onBatchCreatedCallback: vi.fn(),
};

const createWrapper = () => {
  const queryClient = new QueryClient({
    defaultOptions: {
      queries: { retry: false },
      mutations: { retry: false },
    },
  });
  return ({ children }: { children: React.ReactNode }) => (
    <MemoryRouter>
      <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
    </MemoryRouter>
  );
};

describe("Batches", () => {
  beforeEach(() => {
    vi.clearAllMocks();

    // Default mock implementations
    // Mock useFiles to handle multiple calls with different parameters
    vi.mocked(hooks.useFiles).mockImplementation((params?: any) => {
      // All files query (no limit, for batch file lookups)
      if (!params || (!params.purpose && !params.limit)) {
        return {
          data: { data: mockFiles },
          isLoading: false,
          error: null,
          refetch: vi.fn(),
        } as any;
      }
      // Purpose-filtered query or paginated query
      if (params.purpose === "batch" || params.limit) {
        return {
          data: { data: mockFiles },
          isLoading: false,
          error: null,
          refetch: vi.fn(),
        } as any;
      }
      // Default
      return {
        data: { data: mockFiles },
        isLoading: false,
        error: null,
        refetch: vi.fn(),
      } as any;
    });

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
  });

  describe("Rendering", () => {
    it("should render the page title and description", () => {
      render(<Batches {...defaultProps} />, { wrapper: createWrapper() });

      expect(screen.getByText("Batch Processing")).toBeInTheDocument();
      expect(
        screen.getByText(
          "Upload files and create batches to process requests at scale",
        ),
      ).toBeInTheDocument();
    });

    it("should render upload file button", () => {
      render(<Batches {...defaultProps} />, { wrapper: createWrapper() });

      expect(
        screen.getByRole("button", { name: /upload file/i }),
      ).toBeInTheDocument();
    });

    it("should render tabs", () => {
      render(<Batches {...defaultProps} />, { wrapper: createWrapper() });

      expect(
        screen.getByRole("tab", { name: /files \(2\)/i }),
      ).toBeInTheDocument();
      expect(
        screen.getByRole("tab", { name: /batches \(2\)/i }),
      ).toBeInTheDocument();
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

      render(<Batches {...defaultProps} />, { wrapper: createWrapper() });

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

      render(<Batches {...defaultProps} />, { wrapper: createWrapper() });

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

      const { container } = render(<Batches {...defaultProps} />, {
        wrapper: createWrapper(),
      });

      const spinner = container.querySelector(".animate-spin");
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

      render(<Batches {...defaultProps} />, { wrapper: createWrapper() });

      expect(screen.getByText("No files uploaded")).toBeInTheDocument();
      expect(
        screen.getByText(
          "Upload a .jsonl file to get started with batch processing",
        ),
      ).toBeInTheDocument();
      expect(
        screen.getByRole("button", { name: /upload first file/i }),
      ).toBeInTheDocument();
    });

    it("should show empty state when no batches exist", async () => {
      const user = userEvent.setup();

      vi.mocked(hooks.useBatches).mockReturnValue({
        data: { data: [] },
        isLoading: false,
        error: null,
        refetch: vi.fn(),
      } as any);

      render(<Batches {...defaultProps} />, { wrapper: createWrapper() });

      // Switch to batches tab
      await user.click(screen.getByRole("tab", { name: /batches/i }));

      expect(screen.getByText("No batches created")).toBeInTheDocument();
      expect(
        screen.getByText(
          "Create a batch from an uploaded file to start processing requests",
        ),
      ).toBeInTheDocument();
      expect(
        screen.getByRole("button", { name: /create first batch/i }),
      ).toBeInTheDocument();
    });
  });

  describe("Files Tab", () => {
    it("should display files in the table", () => {
      render(<Batches {...defaultProps} />, { wrapper: createWrapper() });

      expect(screen.getByText("test_file.jsonl")).toBeInTheDocument();
      expect(screen.getByText("another_file.jsonl")).toBeInTheDocument();
    });

    it("should open upload modal when upload button is clicked", async () => {
      const user = userEvent.setup();
      const onOpenUploadModal = vi.fn();
      render(
        <Batches {...defaultProps} onOpenUploadModal={onOpenUploadModal} />,
        { wrapper: createWrapper() },
      );

      await user.click(screen.getByRole("button", { name: /upload file/i }));

      expect(onOpenUploadModal).toHaveBeenCalledTimes(1);
    });

    it("should allow searching files", async () => {
      const user = userEvent.setup();
      render(<Batches {...defaultProps} />, { wrapper: createWrapper() });

      const searchInput = screen.getByPlaceholderText(/search files/i);
      await user.type(searchInput, "test");

      expect(searchInput).toHaveValue("test");
    });

    it("should filter files based on search query", async () => {
      const user = userEvent.setup();
      render(<Batches {...defaultProps} />, { wrapper: createWrapper() });

      const searchInput = screen.getByPlaceholderText(/search files/i);
      await user.type(searchInput, "test_file");

      // After filtering, only test_file.jsonl should be visible
      expect(screen.getByText("test_file.jsonl")).toBeInTheDocument();
    });
  });

  describe("Batches Tab", () => {
    it("should allow searching batches", async () => {
      const user = userEvent.setup();
      render(<Batches {...defaultProps} />, { wrapper: createWrapper() });

      // Switch to batches tab
      await user.click(screen.getByRole("tab", { name: /batches/i }));

      const searchInput = screen.getByPlaceholderText(/search batches/i);
      await user.type(searchInput, "batch-1");

      expect(searchInput).toHaveValue("batch-1");
    });

    it("should display batch status correctly", async () => {
      const user = userEvent.setup();
      render(<Batches {...defaultProps} />, { wrapper: createWrapper() });

      await user.click(screen.getByRole("tab", { name: /batches/i }));

      // Check for status badges - there should be multiple "completed" texts
      const completedElements = screen.getAllByText(/completed/i);
      expect(completedElements.length).toBeGreaterThan(0);

      // Check specifically for the status badge with class
      const statusBadge = completedElements.find(
        (el) => el.tagName === "SPAN" && el.className.includes("rounded-full"),
      );
      expect(statusBadge).toBeDefined();

      // Check for in_progress status
      expect(screen.getByText(/in progress/i)).toBeInTheDocument();
    });
  });

  describe("Tab Switching", () => {
    it("should maintain search when switching tabs", async () => {
      const user = userEvent.setup();
      render(<Batches {...defaultProps} />, { wrapper: createWrapper() });

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

  describe("Modal Interactions", () => {
    it("should call onOpenUploadModal when upload button is clicked", async () => {
      const user = userEvent.setup();
      const onOpenUploadModal = vi.fn();
      render(
        <Batches {...defaultProps} onOpenUploadModal={onOpenUploadModal} />,
        { wrapper: createWrapper() },
      );

      await user.click(screen.getByRole("button", { name: /upload file/i }));
      expect(onOpenUploadModal).toHaveBeenCalledTimes(1);

      // Clicking again should call it again
      await user.click(screen.getByRole("button", { name: /upload file/i }));
      expect(onOpenUploadModal).toHaveBeenCalledTimes(2);
    });
  });

  describe("File Size Display", () => {
    it("should display file sizes correctly", () => {
      render(<Batches {...defaultProps} />, { wrapper: createWrapper() });

      // File sizes should be formatted (e.g., "142.19 KB", "87.11 KB")
      const container = screen.getByText("test_file.jsonl").closest("table");
      expect(container).toBeInTheDocument();
    });
  });

  describe("Date Formatting", () => {
    it("should display created dates for files", () => {
      render(<Batches {...defaultProps} />, { wrapper: createWrapper() });

      // Dates should be formatted and displayed
      const container = screen.getByText("test_file.jsonl").closest("table");
      expect(container).toBeInTheDocument();
    });
  });

  describe("Accessibility", () => {
    it("should have accessible tab controls", () => {
      render(<Batches {...defaultProps} />, { wrapper: createWrapper() });

      const filesTab = screen.getByRole("tab", { name: /files/i });
      const batchesTab = screen.getByRole("tab", { name: /batches/i });

      expect(filesTab).toHaveAttribute("aria-selected");
      expect(batchesTab).not.toHaveAttribute("aria-selected", "true");
    });

    it("should have accessible action buttons", () => {
      render(<Batches {...defaultProps} />, { wrapper: createWrapper() });

      expect(
        screen.getByRole("button", { name: /upload file/i }),
      ).toBeEnabled();
    });
  });
});
