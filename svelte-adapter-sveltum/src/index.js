import 'SHIMS';
import path from "node:path";
import { createReadStream } from "node:fs";
import { Readable } from "node:stream";
import { fileURLToPath } from "node:url";

// Load the server itself
import { Server } from "SERVER";

// Load the user specific manifest and other parts
import { base, manifest as svManifest, prerendered as svPrerendered } from "MANIFEST"

const dir = path.dirname(fileURLToPath(import.meta.url));
const asset_dir = `${dir}/client${base}`;

// Create the server
const server = new Server(svManifest);

// Initialize the server
await server.init({
	env: process.env,
	read: (file) => Readable.toWeb(createReadStream(`${asset_dir}/${file}`))
});


export const manifest = {
	app_path: svManifest.appPath,
}

export const prerendered = Array.from(svPrerendered)

/**
 * Handle an HTTP request from the sveltum bridge
 * 
 * @param {*} request 
 * @returns 
 */
export async function handler(request) {
	// Translate the Rust request data into a node request
	const nodeRequest = new Request(request.url, {
		// @ts-ignore
		duplex: 'half',
		method: request.method,
		headers: new Headers(request.headers),
		body: request.body ?? undefined,
	});

	// Pass the request onto svelte to handle
	/** @type {Response} */
	const response = await server.respond(nodeRequest, {
		platform: {},
		getClientAddress: function () {
			throw new Error('Function not implemented.');
		}
	})

	// Create a response object for Rust
	const status = response.status;
	const headers = response.headers;
	const body = await response.arrayBuffer();


	return {
		status,
		headers: Array.from(headers.entries()),
		body
	}
}

