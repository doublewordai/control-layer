import { describe, it, expect, vi, beforeEach } from "vitest";
import {
  render,
  screen,
  waitFor,
  fireEvent,
  within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { CreateBatchModal } from "./CreateBatchModal";
import * as hooks from "../../../api/control-layer/hooks";
import * as contexts from "../../../contexts";

vi.mock("../../../api/control-layer/hooks", () => ({
  useCreateBatch: vi.fn(),
  useUploadFile: vi.fn(),
  useUploadFileWithProgress: vi.fn(),
  useFiles: vi.fn(),
  useFileCostEstimate: vi.fn(),
  useApiKeys: vi.fn(),
  useUser: vi.fn(),
  useConfig: vi.fn(() => ({
    data: {
      docs_url: "https://docs.example.com",
      docs_jsonl_url: "https://docs.example.com/jsonl",
    },
  })),
}));

vi.mock("../../../contexts", () => ({
  useOrganizationContext: vi.fn(),
}));

vi.mock("sonner", () => ({
  toast: {
    success: vi.fn(),
    error: vi.fn(),
  },
}));

// The debounce is timing-only; making it instant keeps the reproduction
// deterministic. The double-upload bug is independent of the 300ms delay — it
// depends only on whether the stale `fileSearchQuery` is cleared before
// `handleSubmit` re-checks `fileToUpload`.
vi.mock("../../../hooks/useDebounce", () => ({
  useDebounce: (value: unknown) => value,
}));

type FileObjectStore = {
  id: string;
  object: "file";
  bytes: number;
  created_at: number;
  expires_at?: number;
  filename: string;
  purpose: "batch";
};

const EXISTING_FILE_A: FileObjectStore = {
  id: "file-existing-1",
  object: "file",
  bytes: 1024,
  created_at: 1730000000,
  expires_at: 1760000000,
  filename: "previous_run.jsonl",
  purpose: "batch",
};

// A second existing file that does NOT contain "run" — used to prove the
// combobox lists every file again once a stale search is cleared.
const EXISTING_FILE_B: FileObjectStore = {
  id: "file-existing-2",
  object: "file",
  bytes: 2048,
  created_at: 1730000001,
  expires_at: 1760000001,
  filename: "other_data.jsonl",
  purpose: "batch",
};

let store: FileObjectStore[];
let nextId: number;
let uploadMutateAsync: ReturnType<typeof vi.fn>;
let createBatchMutateAsync: ReturnType<typeof vi.fn>;

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

const makeJsonlFile = (name: string) =>
  new File(
    [
      JSON.stringify({
        url: "/v1/chat/completions",
        body: { model: "gpt-4o", messages: [] },
      }),
    ],
    name,
    { type: "application/json" },
  );

const getDropZone = (): HTMLElement => {
  const input = document.querySelector(
    'input[type="file"]',
  ) as HTMLInputElement;
  return input.parentElement as HTMLElement;
};

const dropFile = (file: File) => {
  fireEvent.drop(getDropZone(), { dataTransfer: { files: [file] } } as any);
};

const stageViaInput = (file: File) => {
  const input = document.querySelector(
    'input[type="file"]',
  ) as HTMLInputElement;
  Object.defineProperty(input, "files", {
    value: [file],
    configurable: true,
    writable: true,
  });
  fireEvent.change(input);
};

const lastUseFilesSearch = (): string | undefined => {
  const calls = vi.mocked(hooks.useFiles).mock.calls;
  const last = calls[calls.length - 1]?.[0] as
    | { search?: string }
    | undefined;
  return last?.search;
};

const ESTIMATE_BUTTON_NAME = /complete file upload to generate inference cost estimate/i;

beforeEach(() => {
  vi.clearAllMocks();
  store = [{ ...EXISTING_FILE_A }, { ...EXISTING_FILE_B }];
  nextId = 100;

  // Emulates the backend `LOWER(f.name) LIKE '%<search>%'` filter: the refetched
  // list contains the uploaded file only when the search is empty or matches
  // the filename. The store grows as uploads resolve, mirroring the server
  // invalidation + refetch that `useUploadFileWithProgress.onSuccess` triggers.
  vi.mocked(hooks.useFiles).mockImplementation((options?: any) => {
    const search = options?.search as string | undefined;
    const filtered = search
      ? store.filter((f) =>
          f.filename.toLowerCase().includes(search.toLowerCase()),
        )
      : [...store];
    return {
      data: { data: filtered, total_count: filtered.length },
      isLoading: false,
      error: null,
      refetch: vi.fn(),
    } as any;
  });

  uploadMutateAsync = vi.fn().mockImplementation(async ({ data }: any) => {
    const id = `file-uploaded-${nextId++}`;
    const fileObj: FileObjectStore = {
      id,
      object: "file",
      bytes: data.file.size,
      created_at: 1730000000,
      expires_at: 1760000000,
      filename: data.filename || data.file.name,
      purpose: "batch",
    };
    store = [...store, fileObj];
    return fileObj;
  });
  vi.mocked(hooks.useUploadFileWithProgress).mockReturnValue({
    mutateAsync: uploadMutateAsync,
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

  createBatchMutateAsync = vi.fn().mockResolvedValue({});
  vi.mocked(hooks.useCreateBatch).mockReturnValue({
    mutateAsync: createBatchMutateAsync,
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

  vi.mocked(hooks.useFileCostEstimate).mockReturnValue({
    data: { total_requests: 5, total_estimated_cost: "0.0123" },
    isLoading: false,
    error: null,
    refetch: vi.fn(),
  } as any);

  vi.mocked(hooks.useConfig).mockReturnValue({
    data: {
      docs_jsonl_url: "https://docs.example.com/jsonl",
      batches: { allowed_completion_windows: ["24h"] },
    },
    isLoading: false,
    error: null,
    refetch: vi.fn(),
  } as any);

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

  vi.mocked(hooks.useUser).mockReturnValue({
    data: { id: "user-1" },
    isLoading: false,
    error: null,
    refetch: vi.fn(),
  } as any);

  vi.mocked(hooks.useApiKeys).mockReturnValue({
    data: { data: [], total_count: 0 },
    isLoading: false,
    error: null,
    refetch: vi.fn(),
  } as any);

  vi.mocked(contexts.useOrganizationContext).mockReturnValue({
    activeOrganizationId: null,
    activeOrganization: null,
    isOrgContext: false,
    setActiveOrganization: vi.fn(),
  } as any);
});

describe("CreateBatchModal estimate-then-create upload path", () => {
  it("uploads exactly once when a stale non-matching search precedes a drop (reproduction)", async () => {
    const user = userEvent.setup();
    render(<CreateBatchModal isOpen onClose={vi.fn()} />, {
      wrapper: createWrapper(),
    });

    // Type a search substring that does NOT occur in the dropped file's name.
    // Personal context renders only the file combobox (the API-key select is
    // org-only), so the single combobox role identifies the file picker.
    await user.click(screen.getByRole("combobox"));
    await user.type(screen.getByPlaceholderText(/search files/i), "old_data");
    await waitFor(() => expect(lastUseFilesSearch()).toBe("old_data"));

    // Drop a file whose name does not contain "old_data".
    dropFile(makeJsonlFile("fresh_run.jsonl"));

    // The cost-estimate flow uploads the staged file early.
    await user.click(screen.getByRole("button", { name: ESTIMATE_BUTTON_NAME }));
    // Wait until selectedFileId is set and the estimate display replaces the
    // upload button — i.e. the onClick's state updates have committed.
    await waitFor(() =>
      expect(
        screen.queryByRole("button", { name: ESTIMATE_BUTTON_NAME }),
      ).not.toBeInTheDocument(),
    );

    await user.click(screen.getByRole("button", { name: /create batch/i }));

    await waitFor(() =>
      expect(createBatchMutateAsync).toHaveBeenCalledWith(
        expect.objectContaining({ input_file_id: "file-uploaded-100" }),
      ),
    );
    expect(uploadMutateAsync).toHaveBeenCalledTimes(1);
  });
});

describe("CreateBatchModal clears the stale file search", () => {
  it("handleDrop clears the search so useFiles is no longer filtered", async () => {
    const user = userEvent.setup();
    render(<CreateBatchModal isOpen onClose={vi.fn()} />, {
      wrapper: createWrapper(),
    });

    await user.click(screen.getByRole("combobox"));
    await user.type(screen.getByPlaceholderText(/search files/i), "old_data");
    await waitFor(() => expect(lastUseFilesSearch()).toBe("old_data"));

    dropFile(makeJsonlFile("fresh_run.jsonl"));

    expect(lastUseFilesSearch()).toBeUndefined();
  });

  it("handleFileChange clears the search so useFiles is no longer filtered", async () => {
    const user = userEvent.setup();
    render(<CreateBatchModal isOpen onClose={vi.fn()} />, {
      wrapper: createWrapper(),
    });

    await user.click(screen.getByRole("combobox"));
    await user.type(screen.getByPlaceholderText(/search files/i), "old_data");
    await waitFor(() => expect(lastUseFilesSearch()).toBe("old_data"));

    stageViaInput(makeJsonlFile("fresh_run.jsonl"));

    expect(lastUseFilesSearch()).toBeUndefined();
  });

  it("handleRemoveFile re-arms the combobox without a stale search filter", async () => {
    const user = userEvent.setup();
    render(<CreateBatchModal isOpen onClose={vi.fn()} />, {
      wrapper: createWrapper(),
    });

    // "run" matches EXISTING_FILE_A ("previous_run.jsonl") but not
    // EXISTING_FILE_B ("other_data.jsonl").
    await user.click(screen.getByRole("combobox"));
    await user.type(screen.getByPlaceholderText(/search files/i), "run");
    await waitFor(() => expect(lastUseFilesSearch()).toBe("run"));
    expect(
      screen.queryByRole("option", { name: /other_data.jsonl/i }),
    ).not.toBeInTheDocument();

    // Selecting from the combobox sets selectedFileId but does NOT clear the
    // search — so the search stays stale while the selected-file card shows.
    await user.click(
      screen.getByRole("option", { name: /previous_run.jsonl/i }),
    );

    await waitFor(() =>
      expect(screen.getByText("previous_run.jsonl")).toBeInTheDocument(),
    );
    const card = screen
      .getByText("previous_run.jsonl")
      .closest(".bg-gray-50") as HTMLElement;
    await user.click(within(card).getByRole("button"));

    // Removing the file must clear the stale search so the combobox lists
    // every file again, not just the ones matching "run".
    await waitFor(() => expect(lastUseFilesSearch()).toBeUndefined());
    await user.click(screen.getByRole("combobox"));
    expect(
      screen.getByRole("option", { name: /previous_run.jsonl/i }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("option", { name: /other_data.jsonl/i }),
    ).toBeInTheDocument();
  });
});
