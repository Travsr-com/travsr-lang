/// Travsr Phase B — Dart semantic emitter.
///
/// Walks every .dart file under <root-path> using package:analyzer (the same
/// engine that powers `dart analyze`) and emits a JSON document that travsr's
/// Rust plugin converts into graph nodes and edges.
///
/// Usage:
///   dart run bin/emit.dart <root-path> <output-json-path>
///
/// Output JSON schema  (version 1):
/// {
///   "version": 1,
///   "documents": [
///     {
///       "path": "lib/src/foo.dart",       // relative to root-path
///       "definitions": [
///         { "symbol": "<uri>::<qname>", "kind": "class|function|constructor|field|variable", "line": 5, "end_line": 12 }
///       ],
///       "references": [
///         { "symbol": "<uri>::<qname>", "line": 12 }
///       ]
///     }
///   ]
/// }
///
/// Symbol scheme:  "<library-source-uri>::<qualified-name>"
/// e.g.  "package:myapp/src/models.dart::UserModel.fromJson"
/// Both definition sites and reference sites derive the symbol from the
/// resolved Element so they are guaranteed to match across files.

import 'dart:convert';
import 'dart:io';

import 'package:analyzer/dart/analysis/analysis_context_collection.dart';
import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';
import 'package:analyzer/dart/ast/visitor.dart';
import 'package:analyzer/dart/element/element.dart';
import 'package:analyzer/file_system/physical_file_system.dart';
import 'package:path/path.dart' as p;

Future<void> main(List<String> args) async {
  if (args.length < 2) {
    stderr.writeln('usage: emit.dart <root-path> <output-json-path>');
    exit(1);
  }

  final rootPath = p.canonicalize(args[0]);
  final outputPath = args[1];

  if (!Directory(rootPath).existsSync()) {
    stderr.writeln('root path does not exist: $rootPath');
    exit(1);
  }

  final collection = AnalysisContextCollection(
    includedPaths: [rootPath],
    resourceProvider: PhysicalResourceProvider.INSTANCE,
  );

  final documents = <Map<String, dynamic>>[];

  for (final context in collection.contexts) {
    for (final filePath in context.contextRoot.analyzedFiles()) {
      if (!filePath.endsWith('.dart')) continue;
      if (!filePath.startsWith(rootPath)) continue;
      if (_isGenerated(filePath)) continue;

      ResolvedUnitResult result;
      try {
        final r = await context.currentSession.getResolvedUnit(filePath);
        if (r is! ResolvedUnitResult) continue;
        result = r;
      } catch (e) {
        stderr.writeln('warning: could not resolve $filePath: $e');
        continue;
      }

      final relPath = p.relative(filePath, from: rootPath);
      final lineInfo = result.lineInfo;
      final visitor = _ScipVisitor(relPath, lineInfo);
      result.unit.accept(visitor);

      if (visitor.definitions.isNotEmpty || visitor.references.isNotEmpty) {
        documents.add({
          'path': relPath,
          'definitions': visitor.definitions,
          'references': visitor.references,
        });
      }
    }
  }

  final payload = jsonEncode({'version': 1, 'documents': documents});
  File(outputPath).writeAsStringSync(payload);
  stderr.writeln(
    'dart-scip-emitter: ${documents.length} documents written to $outputPath',
  );
}

/// Skip generated files — they bloat the graph with synthetic symbols.
bool _isGenerated(String path) {
  final base = p.basename(path);
  return base.endsWith('.g.dart') ||
      base.endsWith('.freezed.dart') ||
      base.endsWith('.gr.dart') ||
      base.endsWith('.mocks.dart');
}

/// Derives a stable symbol string from a resolved [Element].
///
/// Format: "<library-source-uri>::<qualified-name>"
/// Returns empty string when the element cannot be resolved (e.g. dynamic).
String _elementSymbol(Element? element) {
  if (element == null) return '';
  final lib = element.library;
  if (lib == null) return '';
  final uri = lib.source.uri.toString();
  final name = element.name;
  if (name == null || name.isEmpty) return '';
  final enclosing = element.enclosingElement;
  String? encName;
  if (enclosing is InterfaceElement) {
    encName = enclosing.name;
  } else if (enclosing is ExtensionElement) {
    encName = enclosing.name;
  }
  if (encName != null && encName.isNotEmpty) {
    return '$uri::$encName.$name';
  }
  return '$uri::$name';
}

// ── AST visitor ──────────────────────────────────────────────────────────────

class _ScipVisitor extends RecursiveAstVisitor<void> {
  final String relPath;
  final dynamic lineInfo; // analyzer LineInfo
  final List<Map<String, dynamic>> definitions = [];
  final List<Map<String, dynamic>> references = [];

  _ScipVisitor(this.relPath, this.lineInfo);

  int _line(int offset) => lineInfo.getLocation(offset).lineNumber;

  /// Record a definition. [nameOffset] is the name token's offset (for `line`).
  /// [declEnd] is the exclusive end offset of the full declaration node (for
  /// `end_line`); pass `node.end` and this method subtracts 1 internally.
  void _addDef(Element? element, int nameOffset, String kind, {required int declEnd}) {
    final sym = _elementSymbol(element);
    if (sym.isEmpty) return;
    definitions.add({
      'symbol': sym,
      'kind': kind,
      'line': _line(nameOffset),
      'end_line': _line(declEnd - 1),
    });
  }

  void _addRef(Element? element, int offset) {
    final sym = _elementSymbol(element);
    if (sym.isEmpty) return;
    references.add({'symbol': sym, 'line': _line(offset)});
  }

  // ── Definitions ────────────────────────────────────────────────────────────

  @override
  void visitClassDeclaration(ClassDeclaration node) {
    _addDef(node.declaredElement, node.name.offset, 'class', declEnd: node.end);
    super.visitClassDeclaration(node);
  }

  @override
  void visitMixinDeclaration(MixinDeclaration node) {
    _addDef(node.declaredElement, node.name.offset, 'class', declEnd: node.end);
    super.visitMixinDeclaration(node);
  }

  @override
  void visitExtensionDeclaration(ExtensionDeclaration node) {
    final el = node.declaredElement;
    if (el != null && el.name != null && el.name!.isNotEmpty) {
      _addDef(el, node.extensionKeyword.offset, 'class', declEnd: node.end);
    }
    super.visitExtensionDeclaration(node);
  }

  @override
  void visitEnumDeclaration(EnumDeclaration node) {
    _addDef(node.declaredElement, node.name.offset, 'class', declEnd: node.end);
    super.visitEnumDeclaration(node);
  }

  @override
  void visitEnumConstantDeclaration(EnumConstantDeclaration node) {
    // Enum constants are single-line; end == name line.
    _addDef(node.declaredElement, node.name.offset, 'field', declEnd: node.end);
    super.visitEnumConstantDeclaration(node);
  }

  @override
  void visitFunctionDeclaration(FunctionDeclaration node) {
    // Only top-level functions (not nested).
    if (node.parent is CompilationUnit) {
      _addDef(node.declaredElement, node.name.offset, 'function', declEnd: node.end);
    }
    super.visitFunctionDeclaration(node);
  }

  @override
  void visitMethodDeclaration(MethodDeclaration node) {
    final kind = node.isGetter || node.isSetter ? 'field' : 'function';
    _addDef(node.declaredElement, node.name.offset, kind, declEnd: node.end);
    super.visitMethodDeclaration(node);
  }

  @override
  void visitConstructorDeclaration(ConstructorDeclaration node) {
    _addDef(
      node.declaredElement,
      node.returnType.offset,
      'constructor',
      declEnd: node.end,
    );
    super.visitConstructorDeclaration(node);
  }

  @override
  void visitFieldDeclaration(FieldDeclaration node) {
    for (final v in node.fields.variables) {
      _addDef(v.declaredElement, v.name.offset, 'field', declEnd: node.end);
    }
    super.visitFieldDeclaration(node);
  }

  @override
  void visitTopLevelVariableDeclaration(TopLevelVariableDeclaration node) {
    for (final v in node.variables.variables) {
      _addDef(v.declaredElement, v.name.offset, 'variable', declEnd: node.end);
    }
    super.visitTopLevelVariableDeclaration(node);
  }

  // ── References (call sites) ────────────────────────────────────────────────

  @override
  void visitMethodInvocation(MethodInvocation node) {
    _addRef(
      node.methodName.staticElement,
      node.methodName.offset,
    );
    super.visitMethodInvocation(node);
  }

  @override
  void visitInstanceCreationExpression(InstanceCreationExpression node) {
    _addRef(node.constructorName.staticElement, node.offset);
    super.visitInstanceCreationExpression(node);
  }

  @override
  void visitPrefixedIdentifier(PrefixedIdentifier node) {
    // Static field/method access: ClassName.memberName
    _addRef(node.staticElement, node.identifier.offset);
    super.visitPrefixedIdentifier(node);
  }

}
