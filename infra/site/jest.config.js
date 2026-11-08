// Cap workers: unbounded workers exhaust memory on many-core machines (ts-jest workers reach ~1.6 GB each).
// Coverage is held at 99% of lines and of branches over every source file (bin/ is run by cdk, held/main.ts only
// forwards to held/steps.ts, which is why it holds no logic and is left out of the collection).
module.exports = {
  maxWorkers: 2,
  workerIdleMemoryLimit: '1GB',
  testEnvironment: 'node',
  roots: ['<rootDir>/test'],
  testMatch: ['**/*.test.ts'],
  transform: { '^.+\\.tsx?$': ['ts-jest', { tsconfig: 'tsconfig.json' }] },
  collectCoverageFrom: ['lib/**/*.ts', 'held/**/*.ts', 'bin/**/*.ts'],
  coverageThreshold: { global: { lines: 99, branches: 99 } },
};
