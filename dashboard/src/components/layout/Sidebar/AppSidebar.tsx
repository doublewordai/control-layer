import { NavLink, Link, useNavigate } from "react-router-dom";
import {
  Settings,
  Activity,
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
} from "lucide-react";
import { useUser, useConfig } from "../../../api/control-layer/hooks";
import { UserAvatar } from "../../ui";
import { useAuthorization } from "../../../utils";
import { useAuth } from "../../../contexts/auth";
import { useSettings } from "../../../contexts";
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

interface NavItem {
  path: string;
  icon: React.ComponentType<{ className?: string }>;
  label: string;
  featureFlag?: keyof FeatureFlags;
}

export function AppSidebar() {
  const navigate = useNavigate();
  const { data: currentUser, isLoading: loading } = useUser("current");
  const { canAccessRoute } = useAuthorization();
  const { logout } = useAuth();
  const { isFeatureEnabled } = useSettings();

  const allNavItems: NavItem[] = [
    { path: "/models", icon: Layers, label: "Models" },
    { path: "/endpoints", icon: Server, label: "Endpoints" },
    { path: "/playground", icon: Play, label: "Playground" },
    { path: "/analytics", icon: Activity, label: "Traffic" },
    { path: "/cost-management", icon: DollarSign, label: "Cost Management", featureFlag: "use_billing" },
    { path: "/users-groups", icon: Users, label: "Users & Groups" },
    { path: "/api-keys", icon: Key, label: "API Keys" },
    { path: "/settings", icon: Settings, label: "Settings" },
  ];

  const navItems = allNavItems.filter((item) => {
    // Check feature flag if specified
    if (item.featureFlag && !isFeatureEnabled(item.featureFlag)) {
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
                            ? "!bg-sidebar-accent !text-sidebar-accent-foreground hover:!bg-sidebar-accent"
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
                      <p className="text-xs text-muted-foreground">
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
            <DropdownMenuItem onClick={() => logout()}>
              <LogOut className="w-4 h-4 mr-2" />
              Logout
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenu>
      </SidebarFooter>
    </Sidebar>
  );
}

export function AppLayout({ children }: { children: React.ReactNode }) {
  const { data: config, isLoading: configLoading } = useConfig();

  return (
    <SidebarProvider>
      <div className="flex min-h-screen w-full">
        <AppSidebar />
        <SidebarInset className="flex flex-col flex-1">
          <header className="flex h-16 items-center justify-between border-b px-6">
            <SidebarTrigger />
            <div className="flex items-center gap-6 text-sm text-muted-foreground">
              {!configLoading && config && (
                <>
                  <div className="flex items-center gap-2">
                    <span className="text-muted-foreground/70">Region:</span>
                    <span className="font-medium text-foreground">
                      {config.region}
                    </span>
                  </div>
                  <div className="w-px h-4 bg-border"></div>
                  <div className="flex items-center gap-2">
                    <span className="text-muted-foreground/70">
                      Organization:
                    </span>
                    <span className="font-medium text-foreground">
                      {config.organization}
                    </span>
                  </div>
                  <div className="w-px h-4 bg-border"></div>
                </>
              )}
              <a
                href="https://docs.doubleword.ai/control-layer"
                target="_blank"
                rel="noopener noreferrer"
                className="flex items-center gap-2 text-muted-foreground hover:text-primary transition-colors font-medium"
              >
                <span>Documentation</span>
                <ExternalLink className="w-3 h-3" />
              </a>
            </div>
          </header>
          <main className="flex-1">{children}</main>
        </SidebarInset>
      </div>
    </SidebarProvider>
  );
}
