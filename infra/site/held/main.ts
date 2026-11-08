import { cli } from './cli';
import { realTools } from './tools';

cli(process.argv.slice(2), realTools(), process);
