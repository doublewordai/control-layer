import { useState, useMemo } from "react";
import { useNavigate } from "react-router-dom";
import { useDebounce } from "@/hooks/useDebounce";
import { useServerPagination } from "@/hooks/useServerPagination";
import { useOrganizations, useDeleteOrganization } from "@/api/control-layer/hooks";
import { useAuthorization } from "@/utils";
import type { Organization } from "@/api/control-layer/types";
import { DataTable } from "@/components/ui/data-table";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Plus, Search } from "lucide-react";
import { toast } from "sonner";
import { createOrganizationColumns } from "./columns";
import { CreateOrganizationModal } from "./CreateOrganizationModal";
import { EditOrganizationModal } from "./EditOrganizationModal";

export function Organizations() {
  const navigate = useNavigate();
  const { hasPermission } = useAuthorization();
  const isPlatformManager = hasPermission("users-groups");

  const [search, setSearch] = useState("");
  const debouncedSearch = useDebounce(search, 300);
  const pagination = useServerPagination({ defaultPageSize: 10 });

  const [showCreateModal, setShowCreateModal] = useState(false);
  const [editingOrg, setEditingOrg] = useState<Organization | null>(null);
  const [deletingOrg, setDeletingOrg] = useState<Organization | null>(null);

  const deleteOrg = useDeleteOrganization();

  const { data, isLoading } = useOrganizations({
    skip: pagination.queryParams.skip,
    limit: pagination.queryParams.limit,
    search: debouncedSearch || undefined,
    include: "member_count",
  });

  const columns = useMemo(
    () =>
      createOrganizationColumns({
        onView: (org) => navigate(`/organizations/${org.id}`),
        onEdit: (org) => setEditingOrg(org),
        onDelete: (org) => setDeletingOrg(org),
        canDelete: isPlatformManager,
      }),
    [navigate, isPlatformManager],
  );

  const handleDelete = async () => {
    if (!deletingOrg) return;
    try {
      await deleteOrg.mutateAsync(deletingOrg.id);
      toast.success("Organization deleted");
      setDeletingOrg(null);
    } catch (error) {
      toast.error(
        error instanceof Error
          ? error.message
          : "Failed to delete organization",
      );
    }
  };

  return (
    <div className="p-6 space-y-6">
      <div className="flex items-center justify-between">
        <h1 className="text-2xl font-bold">Organizations</h1>
        <Button onClick={() => setShowCreateModal(true)}>
          <Plus className="h-4 w-4 mr-2" />
          Create Organization
        </Button>
      </div>

      <div className="relative max-w-sm">
        <Search className="absolute left-3 top-1/2 -translate-y-1/2 h-4 w-4 text-muted-foreground" />
        <Input
          placeholder="Search organizations..."
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          className="pl-9"
        />
      </div>

      <DataTable
        columns={columns}
        data={data?.data ?? []}
        isLoading={isLoading}
        paginationMode="server"
        serverPagination={{
          page: pagination.page,
          pageSize: pagination.pageSize,
          totalItems: data?.total_count ?? 0,
          onPageChange: pagination.handlePageChange,
          onPageSizeChange: pagination.handlePageSizeChange,
        }}
      />

      <CreateOrganizationModal
        isOpen={showCreateModal}
        onClose={() => setShowCreateModal(false)}
      />

      <EditOrganizationModal
        isOpen={!!editingOrg}
        onClose={() => setEditingOrg(null)}
        organization={editingOrg}
      />

      <Dialog
        open={!!deletingOrg}
        onOpenChange={(open) => !open && setDeletingOrg(null)}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Delete Organization</DialogTitle>
            <DialogDescription>
              Are you sure you want to delete{" "}
              <strong>
                {deletingOrg?.display_name || deletingOrg?.username}
              </strong>
              ? This action cannot be undone.
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setDeletingOrg(null)}>
              Cancel
            </Button>
            <Button
              variant="destructive"
              onClick={handleDelete}
              disabled={deleteOrg.isPending}
            >
              {deleteOrg.isPending ? "Deleting..." : "Delete"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
