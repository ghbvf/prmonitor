export function isRemoteWebConsolePath(pathname: string): boolean {
  const path = pathname.replace(/\/+$/, "");
  return path === "/ui" || path.startsWith("/ui/");
}
