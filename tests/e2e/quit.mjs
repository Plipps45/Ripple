import { connect, quitApp } from './cdp.mjs';

await quitApp(await connect(Number(process.argv[2] || 9333)));
