import { Refused, Tools, main } from './steps';

/** Runs one command line. A refusal prints its message and sets exit code 2; any other failure propagates. */
export function cli(argv: string[], tools: Tools, process: { exitCode?: string | number | null }, err: (line: string) => void = console.error): void {
  try {
    main(argv, tools);
  } catch (e) {
    if (!(e instanceof Refused)) throw e;
    err(`error: ${e.message}`);
    process.exitCode = 2;
  }
}
