
(() => {

    /**
     * 
     * @param {*} server 
     * @returns 
     */
    function createRequestHandler(server) {
        return async (request) => {
            // Translate the Rust request data into a node request
            const nodeRequest = new Request(request.url, {
                duplex: 'half',
                method: request.method,
                headers: {},
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
            const body = await response.bytes();

            return {
                status,
                headers: Array.from(headers.entries()),
                body
            }
        }
    }

    async function createServer(path) {
        try {
            const pathLib = await import("node:path");

            // Load the server itself
            const { Server } = await import(`file://${path}/server/index.js`)
            // Load the user specific manifest and other parts
            const { base, manifest, prerendered } = await import(`file://${path}/server/manifest.js`)

            const dir = pathLib.dirname(path);
            const asset_dir = `${dir}/client${base}`;

            // Create the server
            const server = new Server(manifest);

            // Initialize the server
            await server.init({
                env: process.env,
                read: (file) => createReadableStream(`${asset_dir}/${file}`)
            });

            return createRequestHandler(server);
        } catch (err) {
            console.error(err);
            throw err;
        }
    }

    return createServer
})()

