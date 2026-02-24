import { useState } from "react";
import { NavLink, Link, useNavigate } from "react-router-dom";
import {
  Settings,
  Box,
  Layers,
  Users,
  Key,
  User,
  Play,
  Server,
  ExternalLink,
  LogOut,
  ChevronUp,
  DollarSign,
  BarChart3,
  LifeBuoy,
  Activity,
} from "lucide-react";
import {
  useUser,
  useConfig,
  useUserBalance,
  useTransactions,
} from "../../../api/control-layer/hooks";
import { UserAvatar } from "../../ui";
import { useAuthorization } from "../../../utils";
import { useAuth } from "../../../contexts/auth";
import { useSettings } from "../../../contexts";
import { SupportRequestModal } from "../../modals";
import type { FeatureFlags } from "../../../contexts/settings/types";
import onwardsLogo from "../../../assets/onwards-logo.svg";
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarGroupContent,
  SidebarHeader,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarProvider,
  SidebarInset,
  SidebarTrigger,
} from "@/components/ui/sidebar";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { formatDollars } from "@/utils/money";

interface NavItem {
  path: string;
  icon: React.ComponentType<{ className?: string }>;
  label: string;
  featureFlag?: keyof FeatureFlags;
  demoOnly?: boolean;
}

export function AppSidebar() {
  const navigate = useNavigate();
  const { data: currentUser, isLoading: loading } = useUser("current");
  const { canAccessRoute } = useAuthorization();
  const { logout } = useAuth();
  const { isFeatureEnabled } = useSettings();
  const [isSupportModalOpen, setIsSupportModalOpen] = useState(false);

  const allNavItems: NavItem[] = [
    { path: "/batches", icon: Box, label: "Batches", demoOnly: false },
    { path: "/models", icon: Layers, label: "Models" },
    { path: "/endpoints", icon: Server, label: "Endpoints" },
    { path: "/playground", icon: Play, label: "Playground" },
    { path: "/analytics", icon: BarChart3, label: "Analytics" },
    { path: "/users-groups", icon: Users, label: "Users & Groups" },
    { path: "/api-keys", icon: Key, label: "API Keys" },
    { path: "/usage", icon: Activity, label: "Usage" },
    { path: "/system", icon: Settings, label: "System" },
  ];

  const navItems = allNavItems.filter((item) => {
    // Check feature flag if specified
    if (item.featureFlag && !isFeatureEnabled(item.featureFlag)) {
      return false;
    }
    // Filter demo-only items when not in demo mode
    if (item.demoOnly && !isFeatureEnabled("demo")) {
      return false;
    }
    // Check route access permissions
    return canAccessRoute(item.path);
  });

  return (
    <Sidebar>
      <SidebarHeader className="border-b border-sidebar-border">
        <Link to="/" className="flex items-center px-2 py-4">
          <img
            src={onwardsLogo}
            alt="Onwards"
            className="h-10 w-auto hover:opacity-80 transition-opacity"
          />
        </Link>
      </SidebarHeader>

      <SidebarContent>
        <SidebarGroup>
          <SidebarGroupContent>
            <SidebarMenu>
              {navItems.map((item) => (
                <SidebarMenuItem key={item.path}>
                  <NavLink to={item.path}>
                    {({ isActive }) => (
                      <SidebarMenuButton
                        isActive={isActive}
                        className={
                          isActive
                            ? "bg-sidebar-accent! text-sidebar-accent-foreground! hover:bg-sidebar-accent!"
                            : "hover:bg-sidebar-border/50"
                        }
                      >
                        <item.icon className="h-4 w-4" />
                        <span>{item.label}</span>
                      </SidebarMenuButton>
                    )}
                  </NavLink>
                </SidebarMenuItem>
              ))}
            </SidebarMenu>
          </SidebarGroupContent>
        </SidebarGroup>
      </SidebarContent>

      <SidebarFooter className="border-t border-sidebar-border">
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <Button
              variant="ghost"
              className="w-full justify-start px-2 py-2 h-auto hover:bg-sidebar-border/50"
            >
              <div className="flex items-center gap-3 w-full">
                {loading ? (
                  <>
                    <div className="w-10 h-10 bg-muted rounded-full animate-pulse"></div>
                    <div className="flex-1 min-w-0">
                      <div className="h-4 bg-muted rounded animate-pulse mb-1 w-24"></div>
                      <div className="h-3 bg-muted rounded animate-pulse w-12"></div>
                    </div>
                  </>
                ) : currentUser ? (
                  <>
                    <UserAvatar user={currentUser} size="lg" />
                    <div className="flex-1 text-left min-w-0">
                      <p className="text-sm font-medium truncate">
                        {currentUser.display_name || currentUser.username}
                      </p>
                      <p className="text-xs text-muted-foreground truncate">
                        {currentUser.email}
                      </p>
                    </div>
                    <ChevronUp className="w-4 h-4 text-muted-foreground" />
                  </>
                ) : (
                  <>
                    <div className="w-10 h-10 bg-muted rounded-full flex items-center justify-center">
                      <User className="w-5 h-5 text-muted-foreground" />
                    </div>
                    <div className="flex-1 text-left min-w-0">
                      <p className="text-sm font-medium">Unknown User</p>
                      <p className="text-xs text-muted-foreground">
                        Error loading
                      </p>
                    </div>
                    <ChevronUp className="w-4 h-4 text-muted-foreground" />
                  </>
                )}
              </div>
            </Button>
          </DropdownMenuTrigger>
          <DropdownMenuContent align="end" className="w-56">
            <DropdownMenuItem onClick={() => navigate("/profile")}>
              <User className="w-4 h-4 mr-2" />
              Profile
            </DropdownMenuItem>
            <DropdownMenuItem onClick={() => navigate("/cost-management")}>
              <DollarSign className="w-4 h-4 mr-2" />
              Billing
            </DropdownMenuItem>
            <DropdownMenuItem onClick={() => setIsSupportModalOpen(true)}>
              <LifeBuoy className="w-4 h-4 mr-2" />
              Support
            </DropdownMenuItem>
            <DropdownMenuItem onClick={() => logout()}>
              <LogOut className="w-4 h-4 mr-2" />
              Logout
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenu>
      </SidebarFooter>

      <SupportRequestModal
        isOpen={isSupportModalOpen}
        onClose={() => setIsSupportModalOpen(false)}
      />
    </Sidebar>
  );
}

export function AppLayout({ children }: { children: React.ReactNode }) {
  const { user } = useAuth();
  const { data: config, isLoading: configLoading } = useConfig();
  const { isFeatureEnabled } = useSettings();
  const isDemoMode = isFeatureEnabled("demo");
  const [isSupportModalOpen, setIsSupportModalOpen] = useState(false);

  // Fetch balance and transactions
  const { data: balance = 0 } = useUserBalance(user?.id || "");
  const { data: transactionsData } = useTransactions({
    userId: user?.id || "",
  });

  // Calculate current balance
  // In demo mode, use page_start_balance from transactions response; otherwise use user balance
  const currentBalance =
    isDemoMode && transactionsData?.page_start_balance !== undefined
      ? transactionsData.page_start_balance
      : balance;

  return (
    <SidebarProvider>
      <div className="flex min-h-screen w-full">
        <AppSidebar />
        <SidebarInset className="flex flex-col flex-1">
          <header className="flex h-16 items-center justify-between border-b px-4 md:px-6">
            <SidebarTrigger />
            <div className="flex items-center gap-3 md:gap-6 text-sm text-muted-foreground">
              {!configLoading && config && (
                <>
                  {config.region && (
                    <>
                      <div className="hidden lg:flex items-center gap-2">
                        <span className="text-muted-foreground/70">
                          Region:
                        </span>
                        <span className="font-medium text-foreground">
                          {config.region}
                        </span>
                      </div>
                      <div className="hidden lg:block w-px h-4 bg-border"></div>
                    </>
                  )}
                  {config.organization && (
                    <>
                      <div className="hidden md:flex items-center gap-2">
                        <span className="text-muted-foreground/70">
                          Organization:
                        </span>
                        <span className="font-medium text-foreground">
                          {config.organization}
                        </span>
                      </div>
                      <div className="hidden lg:block w-px h-4 bg-border"></div>
                    </>
                  )}
                  <Link
                    to="/cost-management"
                    className="flex items-center gap-2 text-sm hover:text-primary transition-colors"
                  >
                    <span className="text-gray-600">Balance:</span>
                    <span className="font-semibold text-gray-900">
                      {formatDollars(currentBalance)}
                    </span>
                  </Link>
                  <div className="hidden md:block w-px h-4 bg-border"></div>
                </>
              )}
              <button
                onClick={() => setIsSupportModalOpen(true)}
                className="hidden md:block text-muted-foreground hover:text-primary transition-colors font-medium"
              >
                Request a model
              </button>
              {config?.docs_url && (
                <>
                  <div className="hidden md:block w-px h-4 bg-border"></div>
                <a
                  href={config.docs_url}
                  target="_blank"
                  rel="noopener noreferrer"
                  className="flex items-center gap-2 text-muted-foreground hover:text-primary transition-colors font-medium"
                >
                  <span className="hidden sm:inline">Documentation</span>
                  <span className="sm:hidden">Docs</span>
                  <ExternalLink className="w-3 h-3" />
                </a>
                </>
              )}
            </div>
          </header>
          <main className="flex-1">{children}</main>
        </SidebarInset>
      </div>

      <SupportRequestModal
        isOpen={isSupportModalOpen}
        onClose={() => setIsSupportModalOpen(false)}
        defaultSubject="Model/Feature Request"
      />
    </SidebarProvider>
  );
}
