/** File -> base64 for upload bodies, shared by every upload surface.
 *
 *  FileReader does the encoding natively -- the copies this replaces
 *  built the string one charCode at a time, freezing the main thread
 *  for seconds on large libraries. The size cap matches the backend's
 *  configured default; the backend remains the authoritative gate.
 */

export const UPLOAD_CAP_MB = 50;

export async function fileToBase64(
  file: File,
  capMb: number = UPLOAD_CAP_MB,
): Promise<{ base64: string } | { error: string }> {
  if (file.size > capMb * 1024 * 1024) {
    return { error: `File exceeds ${capMb}MB cap` };
  }
  const dataUrl = await new Promise<string>((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(reader.result as string);
    reader.onerror = () => reject(reader.error);
    reader.readAsDataURL(file);
  }).catch(() => null);
  if (dataUrl === null) return { error: "Failed to read file" };
  const comma = dataUrl.indexOf(",");
  return { base64: dataUrl.slice(comma + 1) };
}
