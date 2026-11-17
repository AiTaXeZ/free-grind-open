import { platform } from "@tauri-apps/plugin-os";
import { fetch } from "@tauri-apps/plugin-http";
import { appCacheDir, join } from "@tauri-apps/api/path";
import { mkdir, writeFile, remove, BaseDirectory } from "@tauri-apps/plugin-fs";
import {
	requestPhotosAuth,
	getPhotosAuthStatus,
	PhotosAuthorizationStatus,
	requestAlbums,
	createAlbum,
	createPhotos,
	createVideos,
	deleteAlbumMedias,
	PHAssetCollectionType,
	PHAssetCollectionSubtype,
} from "@gbyte/tauri-plugin-ios-photos";
import {
	AndroidFs,
	AndroidPublicGeneralPurposeDir,
	AndroidPublicImageDir,
	AndroidPublicVideoDir,
	type AndroidFsUri,
} from "tauri-plugin-android-fs-api";
import { isTauriRuntime } from "./tauriWebSocket";
import { toDataUri } from "./mediaStore";
import * as chatDb from "./chatDb";
import type { DownloadedMediaEntry } from "../types/chat-db";
import { appLog } from "../utils/logger";

const ALBUM_NAME = "Free Grind";
const SAVE_DIR = "fg-media-save";
const FOLDER_NAME = "FreeGrind";

export function isIos(): boolean {
	if (!isTauriRuntime()) return false;
	try {
		return platform() === "ios";
	} catch {
		return false;
	}
}

export function isAndroid(): boolean {
	if (!isTauriRuntime()) return false;
	try {
		return platform() === "android";
	} catch {
		return false;
	}
}

function isDesktopTauri(): boolean {
	return isTauriRuntime() && !isIos() && !isAndroid();
}

async function ensurePhotosAuthorized(): Promise<boolean> {
	let status = await getPhotosAuthStatus();
	if (status !== PhotosAuthorizationStatus.authorized && status !== PhotosAuthorizationStatus.limited) {
		status = await requestPhotosAuth();
	}
	return status === PhotosAuthorizationStatus.authorized || status === PhotosAuthorizationStatus.limited;
}

async function ensureAlbumId(): Promise<string> {
	const albums = await requestAlbums({
		with: PHAssetCollectionType.album,
		subtype: PHAssetCollectionSubtype.albumRegular,
	});
	const existing = albums.find((album) => album.name === ALBUM_NAME);
	if (existing) return existing.id;

	const created = await createAlbum({ title: ALBUM_NAME });
	if (!created) throw new Error("Failed to create photo album");
	return created;
}

/** Fire-and-forget: records a successfully saved file in the local manifest used for storage-usage reporting and "delete all downloaded media". */
function recordDownloadedMediaEntry(entry: Omit<DownloadedMediaEntry, "savedAt">): void {
	void chatDb.insertDownloadedMediaEntry({ ...entry, savedAt: Date.now() }).catch((error) => {
		appLog.warn("[saveMedia] failed to record downloaded-media entry", error);
	});
}

async function ensureFallbackAlbumId(): Promise<string | null> {
	try {
		const smartAlbums = await requestAlbums({
			with: PHAssetCollectionType.smartAlbum,
			subtype: PHAssetCollectionSubtype.smartAlbumUserLibrary,
		});
		if (smartAlbums[0]?.id) return smartAlbums[0].id;
	} catch {
		// Fall through to regular albums lookup.
	}
	try {
		const regularAlbums = await requestAlbums({
			with: PHAssetCollectionType.album,
			subtype: PHAssetCollectionSubtype.any,
		});
		return regularAlbums[0]?.id ?? null;
	} catch {
		return null;
	}
}

async function fetchMediaBytes(url: string): Promise<{ bytes: Uint8Array; contentType: string | null }> {
	if (url.startsWith("data:")) {
		const commaIdx = url.indexOf(",");
		if (commaIdx === -1) {
			throw new Error("Invalid data URI");
		}
		const meta = url.slice(5, commaIdx);
		const payload = url.slice(commaIdx + 1);
		const isBase64 = /;base64$/i.test(meta);
		const contentType = meta.replace(/;base64$/i, "").trim() || null;
		if (isBase64) {
			return { bytes: base64ToBytes(payload), contentType };
		}
		const decoded = decodeURIComponent(payload);
		return { bytes: new TextEncoder().encode(decoded), contentType };
	}

	if (url.startsWith("blob:")) {
		const response = await globalThis.fetch(url);
		if (!response.ok) throw new Error(`Failed to read local media (${response.status})`);
		const bytes = new Uint8Array(await response.arrayBuffer());
		return { bytes, contentType: response.headers.get("content-type") };
	}

	const response = await fetch(url);
	if (!response.ok) throw new Error(`Failed to download media (${response.status})`);
	const bytes = new Uint8Array(await response.arrayBuffer());
	return { bytes, contentType: response.headers.get("content-type") };
}

function mimeTypeFor(type: "image" | "video", extension: string, contentType: string | null): string {
	if (contentType) return contentType.split(";")[0].trim();
	if (type === "video") return extension === "mov" ? "video/quicktime" : "video/mp4";
	return extension === "png" ? "image/png" : "image/jpeg";
}

function base64ToBytes(base64: string): Uint8Array {
	const binary = atob(base64);
	const bytes = new Uint8Array(binary.length);
	for (let i = 0; i < binary.length; i++) {
		bytes[i] = binary.charCodeAt(i);
	}
	return bytes;
}

/** Keeps a conversation id safe to use as a single path segment on every platform. */
function sanitizeFolderSegment(id: string): string {
	return id.replace(/[^a-zA-Z0-9_-]/g, "_");
}

/**
 * Resolves a caller-supplied conversationId/folder-key into the *other
 * participant's* profile id — so a chat's saves land in the exact same
 * folder as saving that person's profile picture directly, instead of a
 * conversationId string (which encodes both participants, e.g.
 * "myId_partnerId") that's meaningless as a folder name and never matches
 * across entry points. If the input isn't a real conversationId (already a
 * bare profile id passed by a non-chat caller, or no local conversation row
 * exists for it), it's used as-is.
 */
async function mediaRelativeDir(conversationId: string | null | undefined): Promise<string> {
	if (!conversationId) {
		return FOLDER_NAME;
	}
	let key = conversationId;
	try {
		const conversation = await chatDb.getConversation(conversationId);
		if (conversation?.otherProfileId) {
			key = conversation.otherProfileId;
		}
	} catch {
		// Lookup failed — fall back to using the input as-is.
	}
	return `${FOLDER_NAME}/${sanitizeFolderSegment(key)}`;
}

function extensionFromMimeType(mimeType: string | null, type: "image" | "video"): string {
	const subtype = mimeType?.split("/")[1]?.split(";")[0]?.trim();
	if (subtype === "jpeg") return "jpg";
	if (subtype === "quicktime") return "mov";
	if (subtype) return subtype;
	return type === "video" ? "mp4" : "jpg";
}

async function saveMediaBytesToGallery(
	bytes: Uint8Array,
	mimeType: string | null,
	type: "image" | "video",
): Promise<boolean> {
	if (!isIos()) return false;

	const authorized = await ensurePhotosAuthorized();
	if (!authorized) {
		appLog.warn("[saveMedia] Photos permission not granted");
		return false;
	}

	const cache = await appCacheDir();
	try {
		await mkdir(SAVE_DIR, { baseDir: BaseDirectory.AppCache, recursive: true });
	} catch (error) {
		appLog.error("[saveMedia] mkdir failed", error);
		throw error;
	}

	const fileName = `${Date.now()}-${Math.random().toString(36).slice(2)}.${extensionFromMimeType(mimeType, type)}`;
	const relativePath = `${SAVE_DIR}/${fileName}`;
	const absolutePath = await join(cache, relativePath);

	try {
		await writeFile(relativePath, bytes, { baseDir: BaseDirectory.AppCache });
	} catch (error) {
		appLog.error("[saveMedia] writeFile failed", error);
		throw error;
	}

	try {
		let targetAlbumId: string | null = null;
		let created: string[] | null = null;
		try {
			targetAlbumId = await ensureAlbumId();
			created = type === "video"
				? await createVideos({ album: targetAlbumId, files: [absolutePath] })
				: await createPhotos({ album: targetAlbumId, files: [absolutePath] });
		} catch (primaryError) {
			const fallbackAlbumId = await ensureFallbackAlbumId();
			if (!fallbackAlbumId || fallbackAlbumId === targetAlbumId) {
				throw primaryError;
			}
			appLog.warn("[saveMedia] Falling back to user Photos library album", primaryError);
			targetAlbumId = fallbackAlbumId;
			created = type === "video"
				? await createVideos({ album: targetAlbumId, files: [absolutePath] })
				: await createPhotos({ album: targetAlbumId, files: [absolutePath] });
		}
		if (!created || created.length === 0) {
			throw new Error("Photos library did not return a created asset");
		}
		recordDownloadedMediaEntry({
			identifier: created[0],
			platform: "ios",
			albumId: targetAlbumId,
			byteSize: bytes.byteLength,
			mimeType,
			kind: type,
		});
		return true;
	} catch (error) {
		appLog.error("[saveMedia] createPhotos/createVideos failed", error);
		throw error;
	} finally {
		await remove(relativePath, { baseDir: BaseDirectory.AppCache }).catch((error) => {
			appLog.warn("[saveMedia] Failed to clean up temp file", error);
		});
	}
}

async function createAndroidMediaUri(
	relativePath: string,
	mimeType: string,
	type: "image" | "video",
): Promise<AndroidFsUri> {
	const baseDirs =
		type === "video"
			? [AndroidPublicVideoDir.DCIM, AndroidPublicVideoDir.Movies, AndroidPublicGeneralPurposeDir.Download]
			: [AndroidPublicImageDir.DCIM, AndroidPublicImageDir.Pictures, AndroidPublicGeneralPurposeDir.Download];

	let lastError: unknown;
	for (const dir of baseDirs) {
		try {
			return type === "video"
				? await AndroidFs.createNewPublicVideoFile(dir, relativePath, mimeType, { isPending: true })
				: await AndroidFs.createNewPublicImageFile(dir, relativePath, mimeType, { isPending: true });
		} catch (error) {
			lastError = error;
		}
	}
	throw lastError ?? new Error("Failed to create Android public media entry");
}

async function writeAndroidMediaUri(uri: AndroidFsUri, bytes: Uint8Array): Promise<void> {
	try {
		const writable = await AndroidFs.openWriteFileStream(uri);
		const writer = writable.getWriter();
		try {
			await writer.write(bytes);
			await writer.close();
		} catch (error) {
			await writer.abort(error).catch(() => {});
			throw error;
		}
	} catch (streamError) {
		appLog.warn("[saveMedia] openWriteFileStream failed, falling back to writeFile", streamError);
		await AndroidFs.writeFile(uri, bytes);
	}
}

async function saveMediaBytesToGalleryAndroid(
	bytes: Uint8Array,
	mimeTypeIn: string | null,
	type: "image" | "video",
	conversationId?: string | null,
): Promise<boolean> {
	const extension = extensionFromMimeType(mimeTypeIn, type);
	const mimeType = mimeTypeIn || mimeTypeFor(type, extension, null);
	const fileName = `${Date.now()}-${Math.random().toString(36).slice(2)}.${extension}`;
	const relativePath = `${await mediaRelativeDir(conversationId)}/${fileName}`;

	const uri = await createAndroidMediaUri(relativePath, mimeType, type);

	try {
		await writeAndroidMediaUri(uri, bytes);
		await AndroidFs.setPublicFilePending(uri, false);
		await AndroidFs.scanPublicFile(uri);
		recordDownloadedMediaEntry({
			identifier: JSON.stringify(uri),
			platform: "android",
			albumId: null,
			byteSize: bytes.byteLength,
			mimeType,
			kind: type,
		});
		return true;
	} catch (error) {
		appLog.error("[saveMedia] Android writeFile/scan failed", error);
		await AndroidFs.removeFile(uri).catch(() => {});
		throw error;
	}
}

async function saveMediaBytesToFolderDesktop(
	bytes: Uint8Array,
	mimeType: string | null,
	type: "image" | "video",
	conversationId?: string | null,
): Promise<boolean> {
	const extension = extensionFromMimeType(mimeType, type);
	const fileName = `${Date.now()}-${Math.random().toString(36).slice(2)}.${extension}`;
	const baseDir = BaseDirectory.Download;
	const relativeDir = await mediaRelativeDir(conversationId);
	const relativePath = `${relativeDir}/${fileName}`;

	await mkdir(relativeDir, { baseDir, recursive: true });
	await writeFile(relativePath, bytes, { baseDir });
	recordDownloadedMediaEntry({
		identifier: relativePath,
		platform: "desktop",
		albumId: null,
		byteSize: bytes.byteLength,
		mimeType,
		kind: type,
	});
	return true;
}

/**
 * // Downloads a remote chat-media URL and saves it to the devices photo
 * // library, in a "Free Grind" album. iOS only.
 */
export async function saveMediaToGallery(url: string, type: "image" | "video"): Promise<boolean> {
	if (!isIos()) return false;
	const { bytes, contentType } = await fetchMediaBytes(url);
	return saveMediaBytesToGallery(bytes, contentType, type);
}

/**
 * Saves a remote chat-media URL into the public Downloads collection via
 * MediaStore, in a "FreeGrind/<conversationId>" sub-folder. Android only.
 */
async function saveMediaToGalleryAndroid(
	url: string,
	type: "image" | "video",
	conversationId?: string | null,
): Promise<boolean> {
	const { bytes, contentType } = await fetchMediaBytes(url);
	return saveMediaBytesToGalleryAndroid(bytes, contentType, type, conversationId);
}

/**
 * Saves a remote chat-media URL into the user's Downloads folder, in a
 * "FreeGrind/<conversationId>" sub-folder. Desktop (Windows/macOS/Linux) only.
 */
async function saveMediaToFolderDesktop(
	url: string,
	type: "image" | "video",
	conversationId?: string | null,
): Promise<boolean> {
	const { bytes, contentType } = await fetchMediaBytes(url);
	return saveMediaBytesToFolderDesktop(bytes, contentType, type, conversationId);
}

const BATCH_DELAY_MS = 400;

export type SaveMediaBatchItem = { url: string; type: "image" | "video" };

export type SaveMediaBatchResult = {
	total: number;
	succeeded: number;
	failed: number;
};

function downloadFile(url: string): void {
	const a = document.createElement("a");
	a.href = url;
	a.download = `media-${Date.now()}-${Math.random().toString(36).slice(2)}`;
	a.target = "_blank";
	document.body.appendChild(a);
	a.click();
	document.body.removeChild(a);
}

/**
 * Saves a single media item natively where possible: iOS -> photo library
 * album, Android/desktop -> Downloads/FreeGrind folder. Falls back to a
 * plain browser download/open outside of Tauri (web preview), and also if
 * the native save itself throws (e.g. missing plugin/permission).
 */
export async function saveMediaToDevice(
	url: string,
	type: "image" | "video",
	conversationId?: string | null,
): Promise<boolean> {
	if (isIos()) return saveMediaToGallery(url, type);

	if (isAndroid()) {
		try {
			return await saveMediaToGalleryAndroid(url, type, conversationId);
		} catch (error) {
			appLog.error("[saveMedia] Android save failed, falling back to browser", error);
			downloadFile(url);
			return true;
		}
	}

	if (isDesktopTauri()) {
		try {
			return await saveMediaToFolderDesktop(url, type, conversationId);
		} catch (error) {
			appLog.error("[saveMedia] Desktop save failed, falling back to browser", error);
			downloadFile(url);
			return true;
		}
	}

	downloadFile(url);
	return true;
}

function downloadDataUri(dataUri: string): void {
	const a = document.createElement("a");
	a.href = dataUri;
	a.download = `media-${Date.now()}-${Math.random().toString(36).slice(2)}`;
	a.target = "_blank";
	document.body.appendChild(a);
	a.click();
	document.body.removeChild(a);
}

/**
 * Like saveMediaToDevice, but for media we already have the bytes for
 * locally (base64, from chatDb) rather than a live URL — needed for chat
 * media that may no longer have a working remote URL at all (expired,
 * archived conversation) but is still cached and viewable.
 */
export async function saveMediaBytesToDevice(
	base64: string,
	mimeType: string | null,
	type: "image" | "video",
	conversationId?: string | null,
): Promise<boolean> {
	const bytes = base64ToBytes(base64);

	if (isIos()) return saveMediaBytesToGallery(bytes, mimeType, type);

	if (isAndroid()) {
		try {
			return await saveMediaBytesToGalleryAndroid(bytes, mimeType, type, conversationId);
		} catch (error) {
			appLog.error("[saveMedia] Android save failed, falling back to browser", error);
			downloadDataUri(toDataUri(mimeType, base64));
			return true;
		}
	}

	if (isDesktopTauri()) {
		try {
			return await saveMediaBytesToFolderDesktop(bytes, mimeType, type, conversationId);
		} catch (error) {
			appLog.error("[saveMedia] Desktop save failed, falling back to browser", error);
			downloadDataUri(toDataUri(mimeType, base64));
			return true;
		}
	}

	downloadDataUri(toDataUri(mimeType, base64));
	return true;
}

export type SaveMediaBytesBatchItem = { base64: string; mimeType: string | null; type: "image" | "video" };

/** Bytes-based counterpart to saveMediaBatch, for locally-cached media. */
export async function saveMediaBytesBatch(
	items: SaveMediaBytesBatchItem[],
	onProgress?: (done: number, total: number) => void,
	conversationId?: string | null,
): Promise<SaveMediaBatchResult> {
	let succeeded = 0;
	let failed = 0;

	for (let i = 0; i < items.length; i++) {
		const { base64, mimeType, type } = items[i];
		try {
			const saved = await saveMediaBytesToDevice(base64, mimeType, type, conversationId);
			if (saved) succeeded++;
			else failed++;
		} catch (error) {
			appLog.error("[saveMedia] Failed to save item in batch", error);
			failed++;
		}

		onProgress?.(i + 1, items.length);

		if (i < items.length - 1) {
			await new Promise((resolve) => setTimeout(resolve, BATCH_DELAY_MS));
		}
	}

	return { total: items.length, succeeded, failed };
}

/**
 * Sequentially!!! saves a list of media items, with a tiny bitty delay between each
 * to avoid triggerin the CDN/API.
 */
export async function saveMediaBatch(
	items: SaveMediaBatchItem[],
	onProgress?: (done: number, total: number) => void,
	conversationId?: string | null,
): Promise<SaveMediaBatchResult> {
	let succeeded = 0;
	let failed = 0;

	for (let i = 0; i < items.length; i++) {
		const { url, type } = items[i];
		try {
			const saved = await saveMediaToDevice(url, type, conversationId);
			if (saved) succeeded++;
			else failed++;
		} catch (error) {
			appLog.error("[saveMedia] Failed to save item in batch", error);
			failed++;
		}

		onProgress?.(i + 1, items.length);

		if (i < items.length - 1) {
			await new Promise((resolve) => setTimeout(resolve, BATCH_DELAY_MS));
		}
	}

	return { total: items.length, succeeded, failed };
}

/** Total size and count of every file this app has saved to the device. */
export async function getDownloadedMediaUsage(): Promise<{ count: number; totalBytes: number }> {
	return chatDb.getDownloadedMediaUsage();
}

export type DeleteAllDownloadedMediaResult = { deleted: number; failed: number };

/**
 * Deletes every file this app has ever saved to the device (manual saves
 * and auto-downloads alike) — precisely the tracked entries, nothing the
 * user added to the same album/folder themselves. Platform-specific:
 * iOS deletes tracked Photos assets from the "Free Grind" album,
 * Android removes each tracked MediaStore file, desktop removes each
 * tracked file from Downloads/FreeGrind.
 */
export async function deleteAllDownloadedMedia(): Promise<DeleteAllDownloadedMediaResult> {
	const entries = await chatDb.getDownloadedMediaEntries();
	if (entries.length === 0) {
		return { deleted: 0, failed: 0 };
	}

	let deleted = 0;
	let failed = 0;
	const succeededIdentifiers: string[] = [];

	if (isIos()) {
		const byAlbum = new Map<string, string[]>();
		for (const entry of entries) {
			if (!entry.albumId) {
				failed += 1;
				continue;
			}
			const list = byAlbum.get(entry.albumId) ?? [];
			list.push(entry.identifier);
			byAlbum.set(entry.albumId, list);
		}
		for (const [albumId, identifiers] of byAlbum) {
			try {
				const ok = await deleteAlbumMedias({ album: albumId, identifiers });
				if (ok) {
					deleted += identifiers.length;
					succeededIdentifiers.push(...identifiers);
				} else {
					failed += identifiers.length;
				}
			} catch (error) {
				appLog.error("[saveMedia] Failed to delete iOS album medias", error);
				failed += identifiers.length;
			}
		}
	} else if (isAndroid()) {
		for (const entry of entries) {
			try {
				const uri = JSON.parse(entry.identifier) as AndroidFsUri;
				await AndroidFs.removeFile(uri);
				deleted += 1;
				succeededIdentifiers.push(entry.identifier);
			} catch (error) {
				appLog.error("[saveMedia] Failed to delete Android file", error);
				failed += 1;
			}
		}
	} else if (isDesktopTauri()) {
		for (const entry of entries) {
			try {
				await remove(entry.identifier, { baseDir: BaseDirectory.Download });
				deleted += 1;
				succeededIdentifiers.push(entry.identifier);
			} catch (error) {
				appLog.error("[saveMedia] Failed to delete desktop file", error);
				failed += 1;
			}
		}
	} else {
		// Not a supported native platform — browser downloads aren't tracked
		// or revocable from here, nothing to do.
		return { deleted: 0, failed: 0 };
	}

	if (succeededIdentifiers.length > 0) {
		await chatDb.deleteDownloadedMediaEntries(succeededIdentifiers);
	}

	return { deleted, failed };
}
