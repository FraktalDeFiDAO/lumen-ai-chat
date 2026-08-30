import js from '@eslint/js'
import globals from 'globals'
import typescript from 'typescript-eslint'
import vue from 'eslint-plugin-vue'

/// The frontend's equivalent of clippy at `-D warnings`.
///
/// The Rust side has been gated on rustfmt and clippy from the start while
/// TypeScript and Vue had only `vue-tsc`, which checks types and nothing else.
/// A type checker will not tell you that a promise is never awaited, that a
/// `catch` swallows something it should not, or that a template binds a prop
/// that does not exist.
/// Every tsconfig is listed: the test projects are deliberately not referenced
/// from the root one, so that `npm run build` does not typecheck files the
/// production image never copies.
const typeAware = {
  project: [
    './tsconfig.app.json',
    './tsconfig.node.json',
    './tsconfig.vitest.json',
    './tsconfig.e2e.json',
  ],
  tsconfigRootDir: import.meta.dirname,
  extraFileExtensions: ['.vue'],
}

export default typescript.config(
  {
    ignores: [
      'dist/**',
      'node_modules/**',
      'test-results/**',
      'playwright-report/**',
      'graphify-out/**',
      'backend/**',
      '.claude*/**',
    ],
  },

  // Application and test sources. Type-aware, because the rules worth having
  // here -- floating promises, misused awaits -- cannot be decided without
  // types. Every tsconfig is listed: the test projects are deliberately not
  // referenced from the root one, so that `npm run build` does not typecheck
  // files the production image never copies.
  {
    files: ['**/*.ts'],
    extends: [js.configs.recommended, ...typescript.configs.recommendedTypeChecked],
    languageOptions: { parserOptions: { ...typeAware } },
  },
  // Vue files need the vue parser on the outside and the TypeScript parser on
  // the inside, for the `<script setup lang="ts">` block. Applying the vue
  // config to `.ts` as well would put the wrong parser on plain TypeScript.
  {
    files: ['**/*.vue'],
    extends: [
      js.configs.recommended,
      ...typescript.configs.recommendedTypeChecked,
      ...vue.configs['flat/recommended'],
    ],
    languageOptions: {
      parserOptions: { ...typeAware, parser: typescript.parser },
    },
  },

  // The harness scripts are plain Node, in no tsconfig, and type-aware rules
  // have nothing to work with. They still deserve the correctness rules.
  {
    files: ['**/*.mjs', 'eslint.config.js'],
    extends: [js.configs.recommended],
    languageOptions: {
      globals: { ...globals.node },
      parserOptions: { ecmaVersion: 'latest', sourceType: 'module' },
    },
  },

  {
    rules: {
      // This codebase writes deliberately dense single-line templates.
      // Formatting is not what this gate is for.
      'vue/max-attributes-per-line': 'off',
      'vue/singleline-html-element-content-newline': 'off',
      'vue/html-self-closing': 'off',
      'vue/attributes-order': 'off',
      'vue/multiline-html-element-content-newline': 'off',
      // Optional props whose absence is meaningful: `undefined` model means the
      // catalog has not loaded, which is not the same as any default.
      'vue/require-default-prop': 'off',
      // Several deliberate fallbacks are empty catches, and each carries a
      // comment saying why swallowing is the right answer there.
      'no-empty': ['error', { allowEmptyCatch: true }],
    },
  },

  {
    files: ['src/main.ts', 'src/router/index.ts'],
    rules: {
      '@typescript-eslint/no-unsafe-assignment': 'off',
      '@typescript-eslint/no-unsafe-argument': 'off',
    },
  },

  {
    // Tests stub globals and mock aggressively; the type-aware rules that
    // police production code are noise against a faked `fetch`.
    files: ['**/*.test.ts', 'tests/**/*.ts'],
    rules: {
      // A stub that satisfies an async signature has nothing to await.
      '@typescript-eslint/require-await': 'off',
      '@typescript-eslint/no-unsafe-assignment': 'off',
      '@typescript-eslint/no-unsafe-member-access': 'off',
      '@typescript-eslint/no-unsafe-argument': 'off',
      '@typescript-eslint/no-unsafe-call': 'off',
      '@typescript-eslint/no-unsafe-return': 'off',
      '@typescript-eslint/no-explicit-any': 'off',
    },
  },
)
