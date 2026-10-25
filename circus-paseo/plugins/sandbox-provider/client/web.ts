import { Platform } from "react-native";
import type { InputFile } from "../shared/task.js";

export function chooseFiles(): Promise<InputFile[]> {
  if (Platform.OS !== "web") return Promise.resolve([]);
  return new Promise((resolve, reject) => {
    const input = document.createElement("input");
    input.type = "file";
    input.multiple = true;
    input.addEventListener("cancel", () => resolve([]), { once: true });
    input.addEventListener(
      "change",
      () => {
        const files = Array.from(input.files ?? []);
        if (
          files.length > 64 ||
          files.reduce((sum, file) => sum + file.size, 0) > 8 * 1024 * 1024
        ) {
          reject(new Error("Choose at most 64 files totaling 8 MiB."));
          return;
        }
        void Promise.all(
          files.map(async (file) => {
            const bytes = new Uint8Array(await file.arrayBuffer());
            let binary = "";
            for (const byte of bytes) binary += String.fromCharCode(byte);
            return { path: file.webkitRelativePath || file.name, base64: btoa(binary) };
          }),
        ).then(resolve, reject);
      },
      { once: true },
    );
    input.click();
  });
}

export function downloadFile(bytes: Uint8Array<ArrayBuffer>, path: string): void {
  if (Platform.OS !== "web") throw new Error("Download files from the web or desktop app.");
  const url = URL.createObjectURL(new Blob([bytes], { type: "application/octet-stream" }));
  const link = document.createElement("a");
  link.href = url;
  link.download = path.split("/").at(-1) ?? "artifact";
  link.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}
