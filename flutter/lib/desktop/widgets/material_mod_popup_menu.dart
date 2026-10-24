// 版权所有 2014 The Flutter Authors。保留所有权利。
// 本源代码的使用受 BSD 风格许可证约束，该许可证可在
// LICENSE 文件中找到。

import 'package:flutter/foundation.dart';
import 'package:flutter/rendering.dart';
import 'package:flutter/material.dart';
import 'package:flutter_hbb/common.dart';
import 'package:flutter_hbb/desktop/widgets/menu_button.dart';

// 示例可以假定：
// enum Commands { heroAndScholar, hurricaneCame }
// late bool _heroAndScholar;
// late dynamic _selection;
// late BuildContext context;
// void setState(VoidCallback fn) { }
// enum Menu { itemOne, itemTwo, itemThree, itemFour }

// const Duration _kMenuDuration = Duration(milliseconds: 300);
const Duration _kMenuDuration = Duration(milliseconds: 0);
const double _kMenuCloseIntervalEnd = 2.0 / 3.0;
const double _kMenuHorizontalPadding = 16.0;
const double _kMenuDividerHeight = 16.0;
// const double _kMenuMaxWidth = 5.0 * _kMenuWidthStep;
const double _kMenuMinWidth = 2.0 * _kMenuWidthStep;
const double _kMenuMaxWidth = double.infinity;
// const double _kMenuVerticalPadding = 8.0;
const double _kMenuVerticalPadding = 8.0;
const double _kMenuWidthStep = 0.0;
// const double _kMenuScreenPadding = 8.0;
const double _kMenuScreenPadding = 0.0;
const double _kDefaultIconSize = 24.0;

/// 用于配置 [PopupMenuButton] 如何定位其弹出菜单。
enum PopupMenuPosition {
  /// 菜单定位在锚点之上。
  over,

  /// 菜单定位在锚点之下。
  under,

  // 目前仅支持右侧（TextDirection.ltr）
  /// 菜单定位在锚点的侧上方
  overSide,

  // 目前仅支持右侧（TextDirection.ltr）
  /// 菜单定位在锚点的侧下方
  underSide,
}

/// Material Design 弹出菜单中各条目的基类。
///
/// 弹出菜单组件使用该接口与菜单项进行交互。
/// 要显示弹出菜单，请使用 [showMenu] 函数。若要创建一个
/// 可显示弹出菜单的按钮，可以考虑使用 [PopupMenuButton]。
///
/// 类型 `T` 表示该条目所代表的值的类型。同一菜单中所有
/// 条目的值类型必须保持一致。
///
/// [PopupMenuEntry] 可以代表多个值，例如带有多个图标的一行，
/// 也可以只代表单个值，例如带图标的菜单项（参见
/// [PopupMenuItem]），也可以完全不代表任何值（例如 [PopupMenuDivider]）。
///
/// 另请参阅：
///
///  * [PopupMenuItem]，代表单个值的弹出菜单条目。
///  * [PopupMenuDivider]，仅由一条水平线构成的弹出菜单条目。
///  * [CheckedPopupMenuItem]，带勾选标记的弹出菜单项。
///  * [showMenu]，在指定位置动态显示弹出菜单的方法。
///  * [PopupMenuButton]，一种 [IconButton]，在被点击时
///    自动显示菜单。
abstract class PopupMenuEntry<T> extends StatefulWidget {
  /// 抽象 const 构造函数。该构造函数允许子类提供
  /// const 构造函数，以便它们能用于 const 表达式。
  const PopupMenuEntry({Key? key}) : super(key: key);

  /// 该条目占用的垂直空间大小。
  ///
  /// 调用 [showMenu] 方法时，如果提供了
  /// `initialValue` 参数，则使用该值来确定此
  /// 条目在将选中条目对齐到给定 `position` 时的位置。否则
  /// 忽略该值。
  double get height;

  /// 该条目是否代表某个特定的值。
  ///
  /// [showMenu] 调用时使用该方法，将代表
  /// `initialValue` 的条目（如果有）对齐到给定的 `position`，随后
  /// 会对每个条目调用该方法，以确定是否高亮它（若
  /// 该方法返回 true，则该条目的背景色会被设置为
  /// 环境中的 [ThemeData.highlightColor]）。若 `initialValue` 为 null，则
  /// 不会调用该方法。
  ///
  /// 若 [PopupMenuEntry] 代表单个值，则当参数与该值匹配时
  /// 应返回 true。若它代表多个值，则当参数与其中任意一个匹配时
  /// 应返回 true。
  bool represents(T? value);
}

/// Material Design 弹出菜单中的水平分隔线。
///
/// 该组件对 [Divider] 做了适配，使其可用于弹出菜单。
///
/// 另请参阅：
///
///  * [PopupMenuItem]，该组件可用于分隔的条目类型。
///  * [showMenu]，在指定位置动态显示弹出菜单的方法。
///  * [PopupMenuButton]，一种 [IconButton]，在被点击时
///    自动显示菜单。
class PopupMenuDivider extends PopupMenuEntry<Never> {
  /// 为弹出菜单创建水平分隔线。
  ///
  /// 默认情况下，分隔线的高度为 16 逻辑像素。
  const PopupMenuDivider({Key? key, this.height = _kMenuDividerHeight})
      : super(key: key);

  /// 分隔线条目的高度。
  ///
  /// 默认为 16 像素。
  @override
  final double height;

  @override
  bool represents(void value) => false;

  @override
  State<PopupMenuDivider> createState() => _PopupMenuDividerState();
}

class _PopupMenuDividerState extends State<PopupMenuDivider> {
  @override
  Widget build(BuildContext context) => Divider(height: widget.height);
}

// 该组件的存在只是为了让 _PopupMenuRoute 能够保存
// 每个菜单项的尺寸。这些尺寸由 _PopupMenuRouteLayout 用来计算
// 菜单原点的 y 坐标，从而使选中的菜单
// 项的中心与其 PopupMenuButton 的中心对齐。
class _MenuItem extends SingleChildRenderObjectWidget {
  const _MenuItem({
    Key? key,
    required this.onLayout,
    required Widget? child,
  }) : super(key: key, child: child);

  final ValueChanged<Size> onLayout;

  @override
  RenderObject createRenderObject(BuildContext context) {
    return _RenderMenuItem(onLayout);
  }

  @override
  void updateRenderObject(
      BuildContext context, covariant _RenderMenuItem renderObject) {
    renderObject.onLayout = onLayout;
  }
}

class _RenderMenuItem extends RenderShiftedBox {
  _RenderMenuItem(this.onLayout, [RenderBox? child]) : super(child);

  ValueChanged<Size> onLayout;

  @override
  Size computeDryLayout(BoxConstraints constraints) {
    if (child == null) {
      return Size.zero;
    }
    return child!.getDryLayout(constraints);
  }

  @override
  void performLayout() {
    if (child == null) {
      size = Size.zero;
    } else {
      child!.layout(constraints, parentUsesSize: true);
      size = constraints.constrain(child!.size);
      final BoxParentData childParentData = child!.parentData! as BoxParentData;
      childParentData.offset = Offset.zero;
    }
    onLayout(size);
  }
}

/// Material Design 弹出菜单中的一个条目。
///
/// 要显示弹出菜单，请使用 [showMenu] 函数。若要创建一个
/// 可显示弹出菜单的按钮，可以考虑使用 [PopupMenuButton]。
///
/// 若要在弹出菜单项旁显示勾选标记，可以考虑使用
/// [CheckedPopupMenuItem]。
///
/// 通常，[PopupMenuItem] 的 [child] 是一个 [Text] 组件。更复杂的
/// 带图标的菜单可以使用 [ListTile]。默认情况下，
/// [PopupMenuItem] 的高度为 [kMinInteractiveDimension] 像素。如果你使用
/// 高度不同的组件，必须在 [height] 属性中指定其高度。
///
/// {@tool snippet}
///
/// 这里在弹出菜单项中使用了 [Text] 组件。`Menu` 类型
/// 是一个枚举，此处未显示。
///
/// ```dart
/// const PopupMenuItem<Menu>(
///   value: Menu.itemOne,
///   child: Text('Item 1'),
/// )
/// ```
/// {@end-tool}
///
/// 有关本示例如何用于完整菜单，请参阅 [PopupMenuButton] 处的示例，
/// 有关一种保持文字对齐的方法，请参阅 [CheckedPopupMenuItem] 处的示例，
/// 即让使用 [Text] 组件的 [PopupMenuItem] 在其 [child]
/// 槽中的文字与 [CheckedPopupMenuItem] 的文字对齐，或与 [PopupMenuItem]
/// 在 [child] 槽中使用 [ListTile] 时的文字对齐。
///
/// 另请参阅：
///
///  * [PopupMenuDivider]，可用于分隔各个条目。
///  * [CheckedPopupMenuItem]，带勾选标记的 [PopupMenuItem] 变体。
///  * [showMenu]，在指定位置动态显示弹出菜单的方法。
///  * [PopupMenuButton]，一种 [IconButton]，在被点击时
///    自动显示菜单。
class PopupMenuItem<T> extends PopupMenuEntry<T> {
  /// 为弹出菜单创建一个条目。
  ///
  /// 默认情况下，该条目处于 [enabled] 状态。
  ///
  /// `enabled` 和 `height` 参数不能为 null。
  const PopupMenuItem({
    Key? key,
    this.value,
    this.onTap,
    this.enabled = true,
    this.height = kMinInteractiveDimension,
    this.padding,
    this.textStyle,
    this.mouseCursor,
    required this.child,
  }) : super(key: key);

  /// 选中该条目时由 [showMenu] 返回的值。
  final T? value;

  /// 点击该菜单项时调用。
  final VoidCallback? onTap;

  /// 是否允许用户选择该条目。
  ///
  /// 默认为 true。若为 false，则该条目不会响应
  /// 触摸。
  final bool enabled;

  /// 菜单项的最小高度。
  ///
  /// 默认为 [kMinInteractiveDimension] 像素。
  @override
  final double height;

  /// 菜单项的内边距。
  ///
  /// 注意 [height] 可能会与所应用的内边距相互影响。例如，
  /// 如果提供的 [height] 大于内边距与 [child] 之和的高度，
  /// 则内边距的效果将不可见。
  ///
  /// 为 null 时，水平内边距默认左右各为 16.0。
  final EdgeInsets? padding;

  /// 弹出菜单项的文本样式。
  ///
  /// 若该属性为 null，则使用 [PopupMenuThemeData.textStyle]。
  /// 若 [PopupMenuThemeData.textStyle] 也为 null，则使用 [TextTheme.titleMedium]
  /// ，即 [ThemeData.textTheme] 中的该样式。
  final TextStyle? textStyle;

  /// {@template flutter.material.popupmenu.mouseCursor}
  /// 鼠标指针进入或悬停在
  /// 该组件上时使用的光标。
  ///
  /// 若 [mouseCursor] 为 [MaterialStateProperty<MouseCursor>]，
  /// 则以下 [MaterialState] 使用 [MaterialStateProperty.resolve]：
  ///
  ///  * [MaterialState.hovered]。
  ///  * [MaterialState.focused]。
  ///  * [MaterialState.disabled]。
  /// {@endtemplate}
  ///
  /// 若为 null，则使用 [PopupMenuThemeData.mouseCursor] 的值。若
  /// 该值也为 null，则使用 [MaterialStateMouseCursor.clickable]。
  final MouseCursor? mouseCursor;

  /// 位于树中该组件下方的组件。
  ///
  /// 通常为单行的 [ListTile]（用于带图标的菜单）或 [Text]。会为
  /// 子组件应用合适的 [DefaultTextStyle]。无论在哪种
  /// 情况下，文字都应足够短，以免换行。
  final Widget? child;

  @override
  bool represents(T? value) => value == this.value;

  @override
  PopupMenuItemState<T, PopupMenuItem<T>> createState() =>
      PopupMenuItemState<T, PopupMenuItem<T>>();
}

/// [PopupMenuItem] 子类的 [State]。
///
/// 默认情况下，它实现了 Material Design 弹出菜单项的
/// 基本样式和布局。
///
/// 可重写 [buildChild] 方法，以精确调整放入
/// 菜单中的内容。默认返回 [PopupMenuItem.child]。
///
/// 可重写 [handleTap] 方法，以精确调整点击该
/// 条目时发生的行为。默认使用 [Navigator.pop] 从菜单路由返回
/// [PopupMenuItem.value]。
///
/// 该类接受两个类型参数。第二个 `W` 是使用此
/// [State] 的 [Widget] 的确切类型。它必须是
/// [PopupMenuItem] 的子类。第一个 `T` 必须与该组件的类型参数匹配，
/// 也就是从此菜单返回的值的类型。
class PopupMenuItemState<T, W extends PopupMenuItem<T>> extends State<W> {
  /// 菜单项的内容。
  ///
  /// 由 [build] 方法使用。
  ///
  /// 默认返回 [PopupMenuItem.child]。重写它可以在菜单条目中
  /// 放置其他内容。
  @protected
  Widget? buildChild() => widget.child;

  /// 用户选择该菜单项时的处理回调。
  ///
  /// 由 [build] 方法插入的 [InkWell] 使用。
  ///
  /// 默认使用 [Navigator.pop] 从菜单路由返回 [PopupMenuItem.value]，
  /// 也就是菜单路由所持有的该值。
  @protected
  void handleTap() {
    widget.onTap?.call();
    if (Navigator.canPop(context)) {
      Navigator.pop<T>(context, widget.value);
    }
  }

  @override
  Widget build(BuildContext context) {
    final ThemeData theme = Theme.of(context);
    final PopupMenuThemeData popupMenuTheme = PopupMenuTheme.of(context);
    TextStyle style = widget.textStyle ??
        popupMenuTheme.textStyle ??
        theme.textTheme.titleMedium!;

    if (!widget.enabled) style = style.copyWith(color: theme.disabledColor);

    Widget item = AnimatedDefaultTextStyle(
      style: style,
      duration: kThemeChangeDuration,
      child: Container(
        alignment: AlignmentDirectional.centerStart,
        constraints: BoxConstraints(minHeight: widget.height),
        padding: widget.padding ??
            const EdgeInsets.symmetric(horizontal: _kMenuHorizontalPadding),
        child: buildChild(),
      ),
    );

    if (!widget.enabled) {
      final bool isDark = theme.brightness == Brightness.dark;
      item = IconTheme.merge(
        data: IconThemeData(opacity: isDark ? 0.5 : 0.38),
        child: item,
      );
    }

    return MergeSemantics(
      child: Semantics(
        enabled: widget.enabled,
        button: true,
        // child: InkWell(
        //   onTap: widget.enabled ? handleTap : null,
        //   canRequestFocus: widget.enabled,
        //   mouseCursor: _EffectiveMouseCursor(
        //       widget.mouseCursor, popupMenuTheme.mouseCursor),
        //   child: item,
        // ),
        child: TextButton(
          onPressed: widget.enabled ? handleTap : null,
          child: item,
        ),
      ),
    );
  }
}

/// Material Design 弹出菜单中带勾选标记的条目。
///
/// 要显示弹出菜单，请使用 [showMenu] 函数。若要创建一个
/// 可显示弹出菜单的按钮，可以考虑使用 [PopupMenuButton]。
///
/// [CheckedPopupMenuItem] 的高度为 kMinInteractiveDimension 像素，
/// 与 [PopupMenuItem] 的默认最小高度一致。水平
/// 布局使用 [ListTile]；勾选标记是一个 [Icons.done] 图标，显示在
/// [ListTile.leading] 位置。
///
/// {@tool snippet}
///
/// 假设存在一个 `Commands` 枚举，它列出了某个弹出菜单中
/// 可能出现的命令，包括 `Commands.heroAndScholar` 和
/// `Commands.hurricaneCame`，并进一步假设存在一个
/// 布尔类型的 `_heroAndScholar` 成员字段。下面的示例展示了一个
/// 菜单，其中一个带勾选标记的菜单项可以切换该布尔值，
/// 另一个不带勾选标记的菜单项用于选择第二个选项。（它还
/// 展示了一条置于两个菜单项之间的分隔线。）
///
/// ```dart
/// PopupMenuButton<Commands>(
///   onSelected: (Commands result) {
///     switch (result) {
///       case Commands.heroAndScholar:
///         setState(() { _heroAndScholar = !_heroAndScholar; });
///         break;
///       case Commands.hurricaneCame:
///         // ...处理飓风选项
///         break;
///       // ...此处处理其他条目
///     }
///   },
///   itemBuilder: (BuildContext context) => <PopupMenuEntry<Commands>>[
///     CheckedPopupMenuItem<Commands>(
///       checked: _heroAndScholar,
///       value: Commands.heroAndScholar,
///       child: const Text('Hero and scholar'),
///     ),
///     const PopupMenuDivider(),
///     const PopupMenuItem<Commands>(
///       value: Commands.hurricaneCame,
///       child: ListTile(leading: Icon(null), title: Text('Bring hurricane')),
///     ),
///     // ...此处列出其他条目
///   ],
/// )
/// ```
/// {@end-tool}
///
/// 特别地，请注意第二个菜单项使用了 [ListTile]，其
/// [ListTile.leading] 位置上带有一个空 [Icon]，以获得与
/// 带勾选标记的条目相同的对齐方式。
///
/// 另请参阅：
///
///  * [PopupMenuItem]，用于选择某个命令的弹出菜单条目（与
///    切换某个值相对）。
///  * [PopupMenuDivider]，仅由一条水平线构成的弹出菜单条目。
///  * [showMenu]，在指定位置动态显示弹出菜单的方法。
///  * [PopupMenuButton]，一种 [IconButton]，在被点击时
///    自动显示菜单。
class CheckedPopupMenuItem<T> extends PopupMenuItem<T> {
  /// 创建带勾选标记的弹出菜单项。
  ///
  /// 默认情况下，该菜单项处于 [enabled] 状态但未勾选。要将该
  /// 项标记为已勾选，请将 [checked] 设为 true。
  ///
  /// `checked` 和 `enabled` 参数不能为 null。
  const CheckedPopupMenuItem({
    Key? key,
    T? value,
    this.checked = false,
    bool enabled = true,
    EdgeInsets? padding,
    double height = kMinInteractiveDimension,
    Widget? child,
  }) : super(
          key: key,
          value: value,
          enabled: enabled,
          padding: padding,
          height: height,
          child: child,
        );

  /// 是否在菜单项旁显示勾选标记。
  ///
  /// 默认为 false。
  ///
  /// 为 true 时显示 [Icons.done] 勾选标记。
  ///
  /// 选中该弹出菜单项时，勾选标记会根据需要淡入或淡出，
  /// 以表示随之而来的新状态。
  final bool checked;

  /// 位于树中该组件下方的组件。
  ///
  /// 通常为 [Text]。会为子组件应用合适的 [DefaultTextStyle]。
  /// 文字应足够短，以免换行。
  ///
  /// 该组件被放置在某个 [ListTile] 的 [ListTile.title] 槽中，该 [ListTile] 的
  /// [ListTile.leading] 槽是一个 [Icons.done] 图标。
  @override
  Widget? get child => super.child;

  @override
  PopupMenuItemState<T, CheckedPopupMenuItem<T>> createState() =>
      _CheckedPopupMenuItemState<T>();
}

class _CheckedPopupMenuItemState<T>
    extends PopupMenuItemState<T, CheckedPopupMenuItem<T>>
    with SingleTickerProviderStateMixin {
  static const Duration _fadeDuration = Duration(milliseconds: 150);
  late AnimationController _controller;
  Animation<double> get _opacity => _controller.view;

  @override
  void initState() {
    super.initState();
    _controller = AnimationController(duration: _fadeDuration, vsync: this)
      ..value = widget.checked ? 1.0 : 0.0
      ..addListener(() => setState(() {/* animation changed */}));
  }

  @override
  void handleTap() {
    // 点击时会使勾选标记淡入或淡出。
    if (widget.checked) {
      _controller.reverse();
    } else {
      _controller.forward();
    }
    super.handleTap();
  }

  @override
  Widget buildChild() {
    return ListTile(
      enabled: widget.enabled,
      leading: FadeTransition(
        opacity: _opacity,
        child: Icon(_controller.isDismissed ? null : Icons.done),
      ),
      title: widget.child,
    );
  }
}

class _PopupMenu<T> extends StatelessWidget {
  const _PopupMenu({
    Key? key,
    required this.route,
    required this.semanticLabel,
    this.constraints,
  }) : super(key: key);

  final _PopupMenuRoute<T> route;
  final String? semanticLabel;
  final BoxConstraints? constraints;

  @override
  Widget build(BuildContext context) {
    final double unit = 1.0 /
        (route.items.length +
            1.5); // 1.0 for the width and 0.5 for the last item's fade.
    final List<Widget> children = <Widget>[];
    final PopupMenuThemeData popupMenuTheme = PopupMenuTheme.of(context);

    for (int i = 0; i < route.items.length; i += 1) {
      final double start = (i + 1) * unit;
      final double end = (start + 1.5 * unit).clamp(0.0, 1.0);
      final CurvedAnimation opacity = CurvedAnimation(
        parent: route.animation!,
        curve: Interval(start, end),
      );
      Widget item = route.items[i];
      if (route.initialValue != null &&
          route.items[i].represents(route.initialValue)) {
        item = Container(
          color: Theme.of(context).highlightColor,
          child: item,
        );
      }
      children.add(
        _MenuItem(
          onLayout: (Size size) {
            route.itemSizes[i] = size;
          },
          child: FadeTransition(
            opacity: opacity,
            child: item,
          ),
        ),
      );
    }

    final CurveTween opacity =
        CurveTween(curve: const Interval(0.0, 1.0 / 3.0));
    final CurveTween width = CurveTween(curve: Interval(0.0, unit));
    final CurveTween height =
        CurveTween(curve: Interval(0.0, unit * route.items.length));

    final Widget child = ConstrainedBox(
      constraints: constraints ??
          const BoxConstraints(
            minWidth: _kMenuMinWidth,
            maxWidth: _kMenuMaxWidth,
          ),
      child: IntrinsicWidth(
        stepWidth: _kMenuWidthStep,
        child: Semantics(
          scopesRoute: true,
          namesRoute: true,
          explicitChildNodes: true,
          label: semanticLabel,
          child: SingleChildScrollView(
            padding: const EdgeInsets.symmetric(
              vertical: _kMenuVerticalPadding,
            ),
            controller: ScrollController(),
            child: ListBody(children: children),
          ),
        ),
      ),
    );

    return AnimatedBuilder(
      animation: route.animation!,
      builder: (BuildContext context, Widget? child) {
        return FadeTransition(
          opacity: opacity.animate(route.animation!),
          child: Material(
            shape: route.shape ?? popupMenuTheme.shape,
            color: route.color ?? popupMenuTheme.color,
            type: MaterialType.card,
            elevation: route.elevation ?? popupMenuTheme.elevation ?? 8.0,
            child: Align(
              alignment: AlignmentDirectional.topEnd,
              widthFactor: width.evaluate(route.animation!),
              heightFactor: height.evaluate(route.animation!),
              child: child,
            ),
          ),
        );
      },
      child: child,
    );
  }
}

// 菜单在屏幕上的定位方式。
class _PopupMenuRouteLayout extends SingleChildLayoutDelegate {
  _PopupMenuRouteLayout(
    this.position,
    this.itemSizes,
    this.selectedItemIndex,
    this.textDirection,
    this.padding,
    this.avoidBounds,
  );

  // 底层按钮相对于 overlay 尺寸的矩形区域。
  final RelativeRect position;

  // 每个条目的尺寸在菜单布局时计算，并且在
  // 路由布局之前完成。
  List<Size?> itemSizes;

  // 选中条目的索引；若未指定 PopupMenuButton.initialValue 则为
  // null。
  final int? selectedItemIndex;

  // 是否优先向左或向右展开。
  final TextDirection textDirection;

  // 安全区域的内边距。
  EdgeInsets padding;

  // 应避免与之重叠的矩形列表。不可用的屏幕区域。
  final Set<Rect> avoidBounds;

  // 我们按 position 指定的位置放置子组件，前提是它能容纳在
  // 指定的父级尺寸内，并向内（inset）留出 8 的边距。必要时，我们会调整
  // 子组件的位置，使其能够容纳。

  @override
  BoxConstraints getConstraintsForChild(BoxConstraints constraints) {
    // 菜单在每个
    // 方向上的尺寸最多为 overlay 尺寸减去 8.0 像素。
    return BoxConstraints.loose(constraints.biggest).deflate(
      const EdgeInsets.all(_kMenuScreenPadding) + padding,
    );
  }

  @override
  Offset getPositionForChild(Size size, Size childSize) {
    // size：overlay 的尺寸。
    // childSize：菜单完全展开时的尺寸，由
    // getConstraintsForChild 决定。

    final double buttonHeight = size.height - position.top - position.bottom;
    // 查找理想的垂直位置。
    double y = position.top;
    if (selectedItemIndex != null) {
      double selectedItemOffset = _kMenuVerticalPadding;
      for (int index = 0; index < selectedItemIndex!; index += 1) {
        selectedItemOffset += itemSizes[index]!.height;
      }
      selectedItemOffset += itemSizes[selectedItemIndex!]!.height / 2;
      y = y + buttonHeight / 2.0 - selectedItemOffset;
    }

    // 查找理想的水平位置。
    double x;
    // if (position.left > position.right) {
    //   // 菜单按钮更靠近右边缘，因此向左展开，与右边缘对齐。
    //   x = size.width - position.right - childSize.width;
    // } else if (position.left < position.right) {
    //   // 菜单按钮更靠近左边缘，因此向右展开，与左边缘对齐。
    //   x = position.left;
    // } else {
    // 菜单按钮与两侧边缘距离相等，因此沿阅读方向展开。
    switch (textDirection) {
      case TextDirection.rtl:
        x = size.width - position.right - childSize.width;
        break;
      case TextDirection.ltr:
        x = position.left;
        break;
    }
    //}
    final Offset wantedPosition = Offset(x, y);
    final Offset originCenter = position.toRect(Offset.zero & size).center;
    final Iterable<Rect> subScreens =
        DisplayFeatureSubScreen.subScreensInBounds(
            Offset.zero & size, avoidBounds);
    final Rect subScreen = _closestScreen(subScreens, originCenter);
    return _fitInsideScreen(subScreen, childSize, wantedPosition);
  }

  Rect _closestScreen(Iterable<Rect> screens, Offset point) {
    Rect closest = screens.first;
    for (final Rect screen in screens) {
      if ((screen.center - point).distance <
          (closest.center - point).distance) {
        closest = screen;
      }
    }
    return closest;
  }

  Offset _fitInsideScreen(Rect screen, Size childSize, Offset wantedPosition) {
    double x = wantedPosition.dx;
    double y = wantedPosition.dy;
    // 避免超出以下矩形区域：即距屏幕
    // 每个方向的边缘向内 8.0 像素。
    if (x < screen.left + _kMenuScreenPadding + padding.left) {
      x = screen.left + _kMenuScreenPadding + padding.left;
    } else if (x + childSize.width >
        screen.right - _kMenuScreenPadding - padding.right) {
      x = screen.right - childSize.width - _kMenuScreenPadding - padding.right;
    }
    if (y < screen.top + _kMenuScreenPadding + padding.top) {
      y = _kMenuScreenPadding + padding.top;
    } else if (y + childSize.height >
        screen.bottom - _kMenuScreenPadding - padding.bottom) {
      y = screen.bottom -
          childSize.height -
          _kMenuScreenPadding -
          padding.bottom;
    }

    return Offset(x, y);
  }

  @override
  bool shouldRelayout(_PopupMenuRouteLayout oldDelegate) {
    // 若在旧的和新的 itemSizes 都已初始化的情况下调用，则
    // 预期它们长度相同，因为菜单显示之后
    // 实际上没有办法改变条目列表的长度。
    assert(itemSizes.length == oldDelegate.itemSizes.length);

    return position != oldDelegate.position ||
        selectedItemIndex != oldDelegate.selectedItemIndex ||
        textDirection != oldDelegate.textDirection ||
        !listEquals(itemSizes, oldDelegate.itemSizes) ||
        padding != oldDelegate.padding ||
        !setEquals(avoidBounds, oldDelegate.avoidBounds);
  }
}

class _PopupMenuRoute<T> extends PopupRoute<T> {
  _PopupMenuRoute({
    required this.position,
    required this.items,
    this.menuWrapper,
    this.initialValue,
    this.elevation,
    required this.barrierLabel,
    this.semanticLabel,
    this.shape,
    this.color,
    required this.capturedThemes,
    this.constraints,
  }) : itemSizes = List<Size?>.filled(items.length, null);

  final RelativeRect position;
  final List<PopupMenuEntry<T>> items;
  final MenuWrapper? menuWrapper;
  final List<Size?> itemSizes;
  final T? initialValue;
  final double? elevation;
  final String? semanticLabel;
  final ShapeBorder? shape;
  final Color? color;
  final CapturedThemes capturedThemes;
  final BoxConstraints? constraints;

  @override
  Animation<double> createAnimation() {
    return CurvedAnimation(
      parent: super.createAnimation(),
      curve: Curves.linear,
      reverseCurve: const Interval(0.0, _kMenuCloseIntervalEnd),
    );
  }

  @override
  Duration get transitionDuration => _kMenuDuration;

  @override
  bool get barrierDismissible => true;

  @override
  Color? get barrierColor => null;

  @override
  final String barrierLabel;

  @override
  Widget buildPage(BuildContext context, Animation<double> animation,
      Animation<double> secondaryAnimation) {
    int? selectedItemIndex;
    if (initialValue != null) {
      for (int index = 0;
          selectedItemIndex == null && index < items.length;
          index += 1) {
        if (items[index].represents(initialValue)) selectedItemIndex = index;
      }
    }

    Widget menu = _PopupMenu<T>(
      route: this,
      semanticLabel: semanticLabel,
      constraints: constraints,
    );
    if (this.menuWrapper != null) {
      menu = this.menuWrapper!(menu);
    }
    final MediaQueryData mediaQuery = MediaQuery.of(context);
    return MediaQuery.removePadding(
      context: context,
      removeTop: true,
      removeBottom: true,
      removeLeft: true,
      removeRight: true,
      child: Builder(
        builder: (BuildContext context) {
          return CustomSingleChildLayout(
            delegate: _PopupMenuRouteLayout(
              position,
              itemSizes,
              selectedItemIndex,
              Directionality.of(context),
              mediaQuery.padding,
              _avoidBounds(mediaQuery),
            ),
            child: capturedThemes.wrap(menu),
          );
        },
      ),
    );
  }

  Set<Rect> _avoidBounds(MediaQueryData mediaQuery) {
    return DisplayFeatureSubScreen.avoidBounds(mediaQuery).toSet();
  }
}

class PopupMenu<T> extends StatelessWidget {
  PopupMenu({
    Key? key,
    required this.items,
    this.initialValue,
    this.semanticLabel,
    this.constraints,
  })  : itemSizes = List<Size?>.filled(items.length, null),
        super(key: key);

  final List<PopupMenuEntry<T>> items;
  final List<Size?> itemSizes;
  final T? initialValue;
  final String? semanticLabel;
  final BoxConstraints? constraints;

  Widget _buildMenu(BuildContext context) {
    final List<Widget> children = <Widget>[];
    for (int i = 0; i < items.length; i += 1) {
      Widget item = items[i];
      if (initialValue != null && items[i].represents(initialValue)) {
        item = Container(
          color: Theme.of(context).highlightColor,
          child: item,
        );
      }
      children.add(
        _MenuItem(
          onLayout: (Size size) {
            itemSizes[i] = size;
          },
          child: item,
        ),
      );
    }

    final child = ConstrainedBox(
      constraints: constraints ??
          const BoxConstraints(
            minWidth: _kMenuMinWidth,
            maxWidth: _kMenuMaxWidth,
          ),
      child: IntrinsicWidth(
        stepWidth: _kMenuWidthStep,
        child: Semantics(
          scopesRoute: true,
          namesRoute: true,
          explicitChildNodes: true,
          label: semanticLabel,
          child: SingleChildScrollView(
            padding: const EdgeInsets.symmetric(
              vertical: _kMenuVerticalPadding,
            ),
            controller: ScrollController(),
            child: ListBody(children: children),
          ),
        ),
      ),
    );

    final PopupMenuThemeData popupMenuTheme = PopupMenuTheme.of(context);
    return Material(
      shape: popupMenuTheme.shape,
      color: popupMenuTheme.color,
      type: MaterialType.card,
      elevation: popupMenuTheme.elevation ?? 8.0,
      child: child,
    );
  }

  @override
  Widget build(BuildContext context) {
    int? selectedItemIndex;
    if (initialValue != null) {
      for (int index = 0;
          selectedItemIndex == null && index < items.length;
          index += 1) {
        if (items[index].represents(initialValue)) selectedItemIndex = index;
      }
    }

    return MediaQuery.removePadding(
      context: context,
      removeTop: true,
      removeBottom: true,
      removeLeft: true,
      removeRight: true,
      child: Builder(
        builder: (BuildContext context) {
          return InheritedTheme.capture(from: context, to: context)
              .wrap(_buildMenu(context));
        },
      ),
    );
  }
}

/// 在 `position` 处显示包含 `items` 的弹出菜单。
///
/// `items` 不能为 null 且不能为空。
///
/// 若指定了 `initialValue`，则第一个值匹配的条目
/// 会被高亮，`position` 的值给出一个矩形，其
/// 垂直中心将与被高亮条目的垂直中心对齐
/// （在可能的情况下）。
///
/// 若未指定 `initialValue`，则菜单顶部将与
/// `position` 矩形的顶部对齐。
///
/// 在两种情况下，如有必要都会调整菜单位置，使其能够显示在
/// 屏幕上。
///
/// 在水平方向上，菜单的位置会使其朝空间
/// 更充足的方向展开。例如，若 `position` 描述的是屏幕
/// 左边缘上的一个矩形，则菜单的左边缘与
/// `position` 的左边缘对齐，菜单向右展开。若
/// `position` 的两条边缘到屏幕另一侧边缘的距离相同，
/// 则使用环境中的 [Directionality] 作为决胜依据，
/// 优先沿阅读方向展开。
///
/// `initialValue` 在 `position` 处的定位是这样实现的：
/// 遍历 `items`，找到第一个满足以下条件的条目：
/// 其 [PopupMenuEntry.represents] 方法对 `initialValue` 返回 true，然后
/// 累加列表中所有在它之前的组件的 [PopupMenuEntry.height] 值，
/// 即可得到偏移量。
///
/// `elevation` 参数指定放置菜单时的 z 坐标。
/// 高度默认为 8，这是弹出菜单的合适
/// 高度。
///
/// `context` 参数用于查找菜单的 [Navigator] 和 [Theme]。
/// 它仅在该方法被调用时使用。对应的
/// 组件可以在弹出菜单关闭之前安全地从树中移除。
///
/// `useRootNavigator` 参数用于确定是否将菜单
/// 推送到距离给定 `context` 最远或最近的 [Navigator]。它
/// 默认为 `false`。
///
/// `semanticLabel` 参数供无障碍框架使用，用于
/// 在菜单打开和关闭时播报屏幕切换。若未
/// 提供该标签，则默认为
/// [MaterialLocalizations.popupMenuLabel]。
///
/// 另请参阅：
///
///  * [PopupMenuItem]，代表单个值的弹出菜单条目。
///  * [PopupMenuDivider]，仅由一条水平线构成的弹出菜单条目。
///  * [CheckedPopupMenuItem]，带勾选标记的弹出菜单项。
///  * [PopupMenuButton]，它提供一个 [IconButton]，通过
///    自动调用此方法显示菜单。
///  * [SemanticsConfiguration.namesRoute]，用于说明边缘触发
///    语义。
Future<T?> showMenu<T>({
  required BuildContext context,
  required RelativeRect position,
  required List<PopupMenuEntry<T>> items,
  MenuWrapper? menuWrapper,
  T? initialValue,
  double? elevation,
  String? semanticLabel,
  ShapeBorder? shape,
  Color? color,
  bool useRootNavigator = false,
  BoxConstraints? constraints,
}) {
  assert(items.isNotEmpty);
  assert(debugCheckHasMaterialLocalizations(context));

  switch (Theme.of(context).platform) {
    case TargetPlatform.iOS:
    case TargetPlatform.macOS:
      break;
    case TargetPlatform.android:
    case TargetPlatform.fuchsia:
    case TargetPlatform.linux:
    case TargetPlatform.windows:
      semanticLabel ??= MaterialLocalizations.of(context).popupMenuLabel;
  }

  final NavigatorState navigator =
      Navigator.of(context, rootNavigator: useRootNavigator);
  return navigator.push(_PopupMenuRoute<T>(
    position: position,
    items: items,
    menuWrapper: menuWrapper,
    initialValue: initialValue,
    elevation: elevation,
    semanticLabel: semanticLabel,
    barrierLabel: MaterialLocalizations.of(context).modalBarrierDismissLabel,
    shape: shape,
    color: color,
    capturedThemes:
        InheritedTheme.capture(from: context, to: navigator.context),
    constraints: constraints,
  ));
}

/// 选中菜单项时调用的回调的签名。其
/// 参数是导致其菜单被关闭的 [PopupMenuItem] 的
/// 值。
///
/// 由 [PopupMenuButton.onSelected] 使用。
typedef PopupMenuItemSelected<T> = void Function(T value);

/// 当 [PopupMenuButton] 被关闭而
/// 未选择任何条目时调用的回调的签名。
///
/// 由 [PopupMenuButton.onCanceled] 使用。
typedef PopupMenuCanceled = void Function();

/// [PopupMenuButton] 使用的签名，用于惰性构建按下按钮时
/// 显示的条目。
///
/// 由 [PopupMenuButton.itemBuilder] 使用。
typedef PopupMenuItemBuilder<T> = List<PopupMenuEntry<T>> Function(
    BuildContext context);

typedef MenuWrapper = Widget Function(Widget child);

/// 按下时显示菜单，当菜单因选中某个条目而关闭时会调用 [onSelected]。
/// 传递给 [onSelected] 的值是被选中
/// 菜单项的值。
///
/// [child] 和 [icon] 可以提供其中之一，但不能同时提供。若提供了 [icon]，
/// 则 [PopupMenuButton] 的行为与 [IconButton] 相同。
///
/// 若两者都为 null，则会创建一个标准的溢出图标（取决于
/// 平台）。
///
/// {@tool dartpad}
/// 本示例展示了一个包含四个条目的菜单，可在枚举的各个
/// 值之间进行选择，并根据所选项设置 `_selectedMenu` 字段
///
/// ** 代码见 examples/api/lib/material/popupmenu/popupmenu.0.dart **
/// {@end-tool}
///
/// 另请参阅：
///
///  * [PopupMenuItem]，代表单个值的弹出菜单条目。
///  * [PopupMenuDivider]，仅由一条水平线构成的弹出菜单条目。
///  * [CheckedPopupMenuItem]，带勾选标记的弹出菜单项。
///  * [showMenu]，在指定位置动态显示弹出菜单的方法。
class PopupMenuButton<T> extends StatefulWidget {
  /// 创建一个显示弹出菜单的按钮。
  ///
  /// [itemBuilder] 参数不能为 null。
  const PopupMenuButton({
    Key? key,
    required this.itemBuilder,
    this.menuWrapper,
    this.initialValue,
    this.onHover,
    this.onSelected,
    this.onCanceled,
    this.tooltip,
    this.elevation,
    this.padding = const EdgeInsets.all(8.0),
    this.child,
    this.splashRadius,
    this.icon,
    this.iconSize,
    this.offset = Offset.zero,
    this.enabled = true,
    this.shape,
    this.color,
    this.enableFeedback,
    this.constraints,
    this.position = PopupMenuPosition.over,
  })  : assert(
          !(child != null && icon != null),
          'You can only pass [child] or [icon], not both.',
        ),
        super(key: key);

  /// 按下按钮时调用，用于创建要显示在菜单中的条目。
  final PopupMenuItemBuilder<T> itemBuilder;

  /// 菜单包装器。
  final MenuWrapper? menuWrapper;

  /// 菜单打开时需要高亮的菜单项的值（如果有）。
  final T? initialValue;

  /// 用户悬停在此按钮上时调用。
  final ValueChanged<bool>? onHover;

  /// 用户从该按钮创建的弹出菜单中选择某个值时调用。
  ///
  /// 如果弹出菜单在没有选择值的情况下被关闭，则会调用 [onCanceled]
  /// ，而不会调用 [onSelected]。
  final PopupMenuItemSelected<T>? onSelected;

  /// 用户在未选择条目的情况下关闭弹出菜单时调用。
  ///
  /// 如果用户选择了某个值，则改为调用 [onSelected]。
  final PopupMenuCanceled? onCanceled;

  /// 描述按下按钮后将要执行的操作的文本。
  ///
  /// 长按该按钮时会显示此文本，它也
  /// 用于无障碍功能。
  final String? tooltip;

  /// 菜单打开时放置的 z 坐标。它控制菜单
  /// 下方阴影的大小。
  ///
  /// 默认为 8，即弹出菜单的合适高度。
  final double? elevation;

  /// 默认与 IconButton 的 8 dp 内边距一致。在某些情况下，尤其是
  /// 该按钮作为列表条目的尾部元素出现时，能够
  /// 将内边距设为零会很有用。
  final EdgeInsetsGeometry padding;

  /// 水波纹半径。
  ///
  /// 若为 null，则使用 [InkWell] 或 [IconButton] 的默认水波纹半径。
  final double? splashRadius;

  /// 若提供，[child] 将作为该按钮使用的组件，
  /// 该按钮将使用 [InkWell] 处理点击。
  final Widget? child;

  /// 若提供，[icon] 将作为该按钮使用的图标，
  /// 该按钮的行为将与 [IconButton] 相同。
  final Widget? icon;

  /// 该偏移量相对于初始位置
  /// （由 [position] 设置）进行应用。
  ///
  /// 未设置时，偏移量默认为 [Offset.zero]。
  final Offset offset;

  /// 该弹出菜单按钮是否可交互。
  ///
  /// 不能为 null，默认为 `true`
  ///
  /// 若为 `true`，按下按钮时会通过显示菜单来响应。
  ///
  /// 若为 `false`，则按钮会使用当前
  /// [Theme] 中的禁用色进行样式化，并且不会响应按压，也不会显示弹出
  /// 菜单，[onSelected]、[onCanceled] 和 [itemBuilder] 也不会被调用。
  ///
  /// 在应用需要显示该按钮、
  /// 但当前没有内容可在菜单中显示的情况下，这会很有用。
  final bool enabled;

  /// 若提供，则用作菜单的形状。
  ///
  /// 若该属性为 null，则使用 [PopupMenuThemeData.shape]。
  /// 若 [PopupMenuThemeData.shape] 也为 null，则使用
  /// [MaterialType.card] 的默认形状。该默认形状是一个带有
  /// BorderRadius.circular(2.0) 圆角边缘的矩形。
  final ShapeBorder? shape;

  /// 若提供，则用作菜单背景色。
  ///
  /// 若该属性为 null，则使用 [PopupMenuThemeData.color]。
  /// 若 [PopupMenuThemeData.color] 也为 null，则使用
  /// Theme.of(context).cardColor。
  final Color? color;

  /// 检测到手势时是否应提供声音和/或触觉反馈。
  ///
  /// 例如，在 Android 上，轻触会发出咔哒声，
  /// 长按会产生短促的震动（当启用了反馈时）。
  ///
  /// 另请参阅：
  ///
  ///  * [Feedback]，用于为特定操作提供平台特定的反馈。
  final bool? enableFeedback;

  /// 若提供，则为 [Icon] 的大小。
  ///
  /// 若该属性为 null，则使用 [IconThemeData.size]。
  /// 若 [IconThemeData.size] 也为 null，则
  /// 默认大小为 24.0 像素。
  final double? iconSize;

  /// 菜单的可选尺寸约束。
  ///
  /// 未指定时，默认为：
  /// ```dart
  /// const BoxConstraints(
  ///   minWidth: 2.0 * 56.0,
  ///   maxWidth: 5.0 * 56.0,
  /// )
  /// ```
  ///
  /// 默认约束确保菜单宽度与 Material Design 规范
  /// 推荐的最大宽度一致。
  /// 指定该参数后即可创建宽度大于
  /// 默认最大宽度的菜单。
  final BoxConstraints? constraints;

  /// 弹出菜单定位在弹出菜单按钮的上方还是下方。
  ///
  /// [offset] 用于改变弹出菜单相对于
  /// 该参数所设置位置的位置。
  ///
  /// 未设置时，位置默认为 [PopupMenuPosition.over]，这会让
  /// 弹出菜单直接显示在创建它的按钮上方。
  final PopupMenuPosition position;

  @override
  PopupMenuButtonState<T> createState() => PopupMenuButtonState<T>();
}

/// [PopupMenuButton] 的 [State]。
///
/// 有关以编程方式打开弹出菜单的方法，请参阅 [showButtonMenu]
/// （按钮状态）。
class PopupMenuButtonState<T> extends State<PopupMenuButton<T>> {
  /// 一种显示弹出菜单的方法，其中的条目来自
  /// [PopupMenuButton.itemBuilder]，并显示在 [PopupMenuButton] 所在位置。
  ///
  /// 默认情况下，当用户点击按钮且 [PopupMenuButton.enabled]
  /// 设为 `true` 时会调用它。此外，你也可以手动调用该方法打开按钮。
  ///
  /// 你可以使用 [GlobalKey] 访问你的 [PopupMenuButtonState]，然后
  /// 用 `globalKey.currentState.showButtonMenu` 显示该按钮的菜单。
  void showButtonMenu() {
    final PopupMenuThemeData popupMenuTheme = PopupMenuTheme.of(context);
    final RenderBox button = context.findRenderObject()! as RenderBox;
    final RenderBox overlay =
        Navigator.of(context).overlay!.context.findRenderObject()! as RenderBox;
    final Offset offset;
    switch (widget.position) {
      case PopupMenuPosition.over:
        offset = widget.offset;
        break;
      case PopupMenuPosition.under:
        offset =
            Offset(0.0, button.size.height - (widget.padding.vertical / 2)) +
                widget.offset;
        break;
      case PopupMenuPosition.overSide:
        offset =
            Offset(button.size.width - (widget.padding.horizontal / 2), 0.0) +
                widget.offset;
        break;
      case PopupMenuPosition.underSide:
        offset = Offset(button.size.width - (widget.padding.horizontal / 2),
                button.size.height - (widget.padding.vertical / 2)) +
            widget.offset;
        break;
    }
    final RelativeRect position = RelativeRect.fromRect(
      Rect.fromPoints(
        button.localToGlobal(offset, ancestor: overlay),
        button.localToGlobal(button.size.bottomRight(Offset.zero) + offset,
            ancestor: overlay),
      ),
      Offset.zero & overlay.size,
    );
    final List<PopupMenuEntry<T>> items = widget.itemBuilder(context);
    // 只有在有内容可显示时才显示菜单
    if (items.isNotEmpty) {
      showMenu<T?>(
        context: context,
        elevation: widget.elevation ?? popupMenuTheme.elevation,
        items: items,
        menuWrapper: widget.menuWrapper,
        initialValue: widget.initialValue,
        position: position,
        shape: widget.shape ?? popupMenuTheme.shape,
        color: widget.color ?? popupMenuTheme.color,
        constraints: widget.constraints,
      ).then<void>((T? newValue) {
        if (!mounted) return null;
        if (newValue == null) {
          widget.onCanceled?.call();
          return null;
        }
        widget.onSelected?.call(newValue);
      });
    }
  }

  bool get _canRequestFocus {
    final NavigationMode mode = MediaQuery.maybeOf(context)?.navigationMode ??
        NavigationMode.traditional;
    switch (mode) {
      case NavigationMode.traditional:
        return widget.enabled;
      case NavigationMode.directional:
        return true;
    }
  }

  @override
  Widget build(BuildContext context) {
    final bool enableFeedback = widget.enableFeedback ??
        PopupMenuTheme.of(context).enableFeedback ??
        true;

    assert(debugCheckHasMaterialLocalizations(context));

    if (widget.child != null) {
      return Tooltip(
        message:
            widget.tooltip ?? MaterialLocalizations.of(context).showMenuTooltip,
        child: InkWell(
          onTap: widget.enabled ? showButtonMenu : null,
          onHover: widget.onHover,
          canRequestFocus: _canRequestFocus,
          enableFeedback: enableFeedback,
          child: widget.child,
        ),
      );
    }

    return MenuButton(
      child: widget.icon ?? Icon(Icons.adaptive.more),
      tooltip:
          widget.tooltip ?? MaterialLocalizations.of(context).showMenuTooltip,
      onPressed: widget.enabled ? showButtonMenu : null,
      enableFeedback: enableFeedback,
      color: MyTheme.button,
      hoverColor: MyTheme.accent,
    );
  }
}

// 该 MaterialStateProperty 会传递给菜单项的 InkWell，后者会
// 针对 MaterialState.disabled、MaterialState.hovered、
// MaterialState.focused 解析该属性。
// ignore: unused_element
class _EffectiveMouseCursor extends MaterialStateMouseCursor {
  const _EffectiveMouseCursor(this.widgetCursor, this.themeCursor);

  final MouseCursor? widgetCursor;
  final MaterialStateProperty<MouseCursor?>? themeCursor;

  @override
  MouseCursor resolve(Set<MaterialState> states) {
    return MaterialStateProperty.resolveAs<MouseCursor?>(
            widgetCursor, states) ??
        themeCursor?.resolve(states) ??
        MaterialStateMouseCursor.clickable.resolve(states);
  }

  @override
  String get debugDescription => 'MaterialStateMouseCursor(PopupMenuItemState)';
}
