export function fmtSize(bytes) {
  const b = Number(bytes) || 0;
  if (b === 0) return "—";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  let n = b;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i++;
  }
  return n.toFixed(i > 1 ? 2 : 0) + " " + units[i];
}
