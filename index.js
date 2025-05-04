import path from "node:path";
import { createReadStream } from "node:fs";
import { Readable } from "node:stream";
import { fileURLToPath } from "node:url";

// Load the server itself
import { Server } from "./server/index.js";

// Load the user specific manifest and other parts
import { base, manifest as svManifest, prerendered as svPrerendered } from "./server/manifest.js"

function createRequestHandler(server) {
    return async (request) => {
        // Translate the Rust request data into a node request
        const nodeRequest = new Request(request.url, {
            duplex: 'half',
            method: request.method,
            headers: new Headers(request.headers),
            body: request.body ?? undefined,
        });

        // Pass the request onto svelte to handle
        /** @type {Response} */
        const response = await server.respond(nodeRequest, {
            platform: {},
            // ...options
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
}

const dir = path.dirname(fileURLToPath(import.meta.url));
const asset_dir = `${dir}/client${base}`;

// Create the server
const server = new Server(svManifest);

// Initialize the server
await server.init({
    env: process.env,
    read: (file) => Readable.toWeb(createReadStream(`${asset_dir}/${file}`))
});


export const handler = createRequestHandler(server);

export const manifest = {
    app_path: svManifest.appPath,
}

export const prerendered = Array.from(svPrerendered)
