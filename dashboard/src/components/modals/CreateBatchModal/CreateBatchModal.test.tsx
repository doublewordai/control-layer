import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { CreateBatchModal } from "./CreateBatchModal";
import * as hooks from "../../../api/control-layer/hooks";

// Mock the hooks
vi.mock("../../../api/control-layer/hooks", () => ({
  useCreateBatch: vi.fn(),
  useUploadFile: vi.fn(),
  useUploadFileWithProgress: vi.fn(),
  useFiles: vi.fn(),
  useFileCostEstimate: vi.fn(),
  useConfig: vi.fn(() => ({
    data: {
      docs_url: "https://docs.example.com",
      docs_jsonl_url: "https://docs.example.com/jsonl",
    },
  })),
}));

// Mock sonner toast
vi.mock("sonner", () => ({
  toast: {
    success: vi.fn(),
    error: vi.fn(),
  },
}));

const mockFile = {
  id: "file-123",
  object: "file" as const,
  bytes: 1024000,
  created_at: 1730995200,
  expires_at: 1765065600,
  filename: "test-batch.jsonl",
  purpose: "batch" as const,
};

const createWrapper = () => {
  const queryClient = new QueryClient({
    defaultOptions: {
      queries: { retry: false, gcTime: 0, staleTime: 0 },
      mutations: { retry: false },
    },
  });
  return ({ children }: { children: React.ReactNode }) => (
    <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
  );
};

describe("CreateBatchModal", () => {
  beforeEach(() => {
    vi.clearAllMocks();

    // Default mock for useUploadFile
    vi.mocked(hooks.useUploadFile).mockReturnValue({
      mutateAsync: vi.fn(),
      isPending: false,
      isError: false,
      error: null,
      isSuccess: false,
      data: undefined,
      mutate: vi.fn(),
      reset: vi.fn(),
      status: "idle",
      context: undefined,
      failureCount: 0,
      failureReason: null,
      isIdle: true,
      isPaused: false,
      submittedAt: 0,
      variables: undefined,
    } as any);

    // Default mock for useUploadFileWithProgress
    vi.mocked(hooks.useUploadFileWithProgress).mockReturnValue({
      mutateAsync: vi.fn(),
      isPending: false,
      isError: false,
      error: null,
      isSuccess: false,
      data: undefined,
      mutate: vi.fn(),
      reset: vi.fn(),
      status: "idle",
      context: undefined,
      failureCount: 0,
      failureReason: null,
      isIdle: true,
      isPaused: false,
      submittedAt: 0,
      variables: undefined,
    } as any);

    // Default mock for useFiles
    vi.mocked(hooks.useFiles).mockReturnValue({
      data: { data: [] },
      isLoading: false,
      error: null,
      refetch: vi.fn(),
    } as any);

    // Default mock for useConfig
    vi.mocked(hooks.useConfig).mockReturnValue({
      data: {
        batches: {
          allowed_completion_windows: ["Standard (24h)"],
        },
      },
      isLoading: false,
      error: null,
      refetch: vi.fn(),
    } as any);

    // Default mock for useFileCostEstimate
    vi.mocked(hooks.useFileCostEstimate).mockReturnValue({
      data: null,
      isLoading: false,
      error: null,
      refetch: vi.fn(),
    } as any);
  });

  describe("Basic interactions", () => {
    it("should close modal when Cancel button is clicked", async () => {
      const user = userEvent.setup();
      const onClose = vi.fn();
      const mutateAsync = vi.fn().mockResolvedValue({});

      vi.mocked(hooks.useCreateBatch).mockReturnValue({
        mutateAsync,
        isPending: false,
        isError: false,
        error: null,
        isSuccess: false,
        data: undefined,
        mutate: vi.fn(),
        reset: vi.fn(),
        status: "idle",
        context: undefined,
        failureCount: 0,
        failureReason: null,
        isIdle: true,
        isPaused: false,
        submittedAt: 0,
        variables: undefined,
      } as any);

      render(
        <CreateBatchModal
          isOpen={true}
          onClose={onClose}
          preselectedFile={mockFile}
        />,
        { wrapper: createWrapper() },
      );

      // Find and click the Cancel button
      const cancelButton = screen.getByRole("button", { name: /cancel/i });
      await user.click(cancelButton);

      // Verify onClose was called
      expect(onClose).toHaveBeenCalled();
      // Verify mutation was NOT called
      expect(mutateAsync).not.toHaveBeenCalled();
    });

    it("should submit when Create Batch button is clicked", async () => {
      const user = userEvent.setup();
      const onClose = vi.fn();
      const onSuccess = vi.fn();
      const mutateAsync = vi.fn().mockResolvedValue({});

      vi.mocked(hooks.useCreateBatch).mockReturnValue({
        mutateAsync,
        isPending: false,
        isError: false,
        error: null,
        isSuccess: false,
        data: undefined,
        mutate: vi.fn(),
        reset: vi.fn(),
        status: "idle",
        context: undefined,
        failureCount: 0,
        failureReason: null,
        isIdle: true,
        isPaused: false,
        submittedAt: 0,
        variables: undefined,
      } as any);

      render(
        <CreateBatchModal
          isOpen={true}
          onClose={onClose}
          onSuccess={onSuccess}
          preselectedFile={mockFile}
        />,
        { wrapper: createWrapper() },
      );

      // Add a description
      const descriptionInput =
        screen.getByPlaceholderText(/Data generation task/i);
      await user.type(descriptionInput, "Test batch");

      // Find and click the Create Batch button
      const createButton = screen.getByRole("button", {
        name: /create batch/i,
      });
      await user.click(createButton);

      // Verify the mutation was called
      await waitFor(() => {
        expect(mutateAsync).toHaveBeenCalledWith({
          input_file_id: "file-123",
          endpoint: "/v1/chat/completions",
          completion_window: "Standard (24h)",
          metadata: {
            batch_description: "Test batch",
          },
        });
      });

      // Verify callbacks were called
      await waitFor(() => {
        expect(onSuccess).toHaveBeenCalled();
        expect(onClose).toHaveBeenCalled();
      });
    });

    it("should disable Create Batch button when no file is selected", async () => {
      const mutateAsync = vi.fn().mockResolvedValue({});

      vi.mocked(hooks.useCreateBatch).mockReturnValue({
        mutateAsync,
        isPending: false,
        isError: false,
        error: null,
        isSuccess: false,
        data: undefined,
        mutate: vi.fn(),
        reset: vi.fn(),
        status: "idle",
        context: undefined,
        failureCount: 0,
        failureReason: null,
        isIdle: true,
        isPaused: false,
        submittedAt: 0,
        variables: undefined,
      } as any);

      render(
        <CreateBatchModal
          isOpen={true}
          onClose={vi.fn()}
          preselectedFile={undefined}
        />,
        { wrapper: createWrapper() },
      );

      // Find the Create Batch button
      const createButton = screen.getByRole("button", {
        name: /create batch/i,
      });

      // Verify it's disabled
      expect(createButton).toBeDisabled();
    });

    it("should disable buttons when mutation is pending", async () => {
      const mutateAsync = vi.fn().mockResolvedValue({});

      vi.mocked(hooks.useCreateBatch).mockReturnValue({
        mutateAsync,
        isPending: true, // Mutation in progress
        isError: false,
        error: null,
        isSuccess: false,
        data: undefined,
        mutate: vi.fn(),
        reset: vi.fn(),
        status: "pending",
        context: undefined,
        failureCount: 0,
        failureReason: null,
        isIdle: false,
        isPaused: false,
        submittedAt: Date.now(),
        variables: undefined,
      } as any);

      render(
        <CreateBatchModal
          isOpen={true}
          onClose={vi.fn()}
          preselectedFile={mockFile}
        />,
        { wrapper: createWrapper() },
      );

      // Find buttons
      const cancelButton = screen.getByRole("button", { name: /cancel/i });
      const createButton = screen.getByRole("button", { name: /creating/i });

      // Verify they're disabled
      expect(cancelButton).toBeDisabled();
      expect(createButton).toBeDisabled();
    });
  });

  describe("Enter key submission", () => {
    it("should submit the form when Enter is pressed in description field", async () => {
      const user = userEvent.setup();
      const onClose = vi.fn();
      const onSuccess = vi.fn();
      const mutateAsync = vi.fn().mockResolvedValue({});

      vi.mocked(hooks.useCreateBatch).mockReturnValue({
        mutateAsync,
        isPending: false,
        isError: false,
        error: null,
        isSuccess: false,
        data: undefined,
        mutate: vi.fn(),
        reset: vi.fn(),
        status: "idle",
        context: undefined,
        failureCount: 0,
        failureReason: null,
        isIdle: true,
        isPaused: false,
        submittedAt: 0,
        variables: undefined,
      } as any);

      render(
        <CreateBatchModal
          isOpen={true}
          onClose={onClose}
          onSuccess={onSuccess}
          preselectedFile={mockFile}
        />,
        { wrapper: createWrapper() },
      );

      // Find and focus the description input - use screen since Dialog renders in a portal
      const descriptionInput =
        screen.getByPlaceholderText(/Data generation task/i);
      await user.click(descriptionInput);
      await user.type(descriptionInput, "Test batch description");

      // Press Enter
      await user.keyboard("{Enter}");

      // Verify the mutation was called
      await waitFor(() => {
        expect(mutateAsync).toHaveBeenCalledWith({
          input_file_id: "file-123",
          endpoint: "/v1/chat/completions",
          completion_window: "Standard (24h)",
          metadata: {
            batch_description: "Test batch description",
          },
        });
      });

      // Verify callbacks were called
      await waitFor(() => {
        expect(onSuccess).toHaveBeenCalled();
        expect(onClose).toHaveBeenCalled();
      });
    });

    it("should not submit when Enter is pressed if no file is selected", async () => {
      const user = userEvent.setup();
      const onClose = vi.fn();
      const mutateAsync = vi.fn().mockResolvedValue({});

      vi.mocked(hooks.useCreateBatch).mockReturnValue({
        mutateAsync,
        isPending: false,
        isError: false,
        error: null,
        isSuccess: false,
        data: undefined,
        mutate: vi.fn(),
        reset: vi.fn(),
        status: "idle",
        context: undefined,
        failureCount: 0,
        failureReason: null,
        isIdle: true,
        isPaused: false,
        submittedAt: 0,
        variables: undefined,
      } as any);

      render(
        <CreateBatchModal
          isOpen={true}
          onClose={onClose}
          preselectedFile={undefined}
        />,
        { wrapper: createWrapper() },
      );

      // Find and focus the description input - use screen since Dialog renders in a portal
      const descriptionInput =
        screen.getByPlaceholderText(/Data generation task/i);
      await user.click(descriptionInput);
      await user.type(descriptionInput, "Test description");

      // Press Enter
      await user.keyboard("{Enter}");

      // Verify the mutation was NOT called
      expect(mutateAsync).not.toHaveBeenCalled();
    });

    it("should not submit when Enter is pressed if mutation is pending", async () => {
      const user = userEvent.setup();
      const onClose = vi.fn();
      const mutateAsync = vi.fn().mockResolvedValue({});

      vi.mocked(hooks.useCreateBatch).mockReturnValue({
        mutateAsync,
        isPending: true, // Mutation in progress
        isError: false,
        error: null,
        isSuccess: false,
        data: undefined,
        mutate: vi.fn(),
        reset: vi.fn(),
        status: "pending",
        context: undefined,
        failureCount: 0,
        failureReason: null,
        isIdle: false,
        isPaused: false,
        submittedAt: Date.now(),
        variables: undefined,
      } as any);

      render(
        <CreateBatchModal
          isOpen={true}
          onClose={onClose}
          preselectedFile={mockFile}
        />,
        { wrapper: createWrapper() },
      );

      // Find and focus the description input - use screen since Dialog renders in a portal
      const descriptionInput =
        screen.getByPlaceholderText(/Data generation task/i);
      await user.click(descriptionInput);
      await user.type(descriptionInput, "Test description");

      // Press Enter
      await user.keyboard("{Enter}");

      // Verify the mutation was NOT called again
      expect(mutateAsync).not.toHaveBeenCalled();
    });

    it("should submit with empty description when Enter is pressed", async () => {
      const user = userEvent.setup();
      const onClose = vi.fn();
      const onSuccess = vi.fn();
      const mutateAsync = vi.fn().mockResolvedValue({});

      vi.mocked(hooks.useCreateBatch).mockReturnValue({
        mutateAsync,
        isPending: false,
        isError: false,
        error: null,
        isSuccess: false,
        data: undefined,
        mutate: vi.fn(),
        reset: vi.fn(),
        status: "idle",
        context: undefined,
        failureCount: 0,
        failureReason: null,
        isIdle: true,
        isPaused: false,
        submittedAt: 0,
        variables: undefined,
      } as any);

      render(
        <CreateBatchModal
          isOpen={true}
          onClose={onClose}
          onSuccess={onSuccess}
          preselectedFile={mockFile}
        />,
        { wrapper: createWrapper() },
      );

      // Find and focus the description input (don't type anything) - use screen since Dialog renders in a portal
      const descriptionInput =
        screen.getByPlaceholderText(/Data generation task/i);
      await user.click(descriptionInput);

      // Press Enter without typing
      await user.keyboard("{Enter}");

      // Verify the mutation was called without metadata
      await waitFor(() => {
        expect(mutateAsync).toHaveBeenCalledWith({
          input_file_id: "file-123",
          endpoint: "/v1/chat/completions",
          completion_window: "Standard (24h)",
          metadata: undefined,
        });
      });

      // Verify callbacks were called
      await waitFor(() => {
        expect(onSuccess).toHaveBeenCalled();
        expect(onClose).toHaveBeenCalled();
      });
    });
  });
});
