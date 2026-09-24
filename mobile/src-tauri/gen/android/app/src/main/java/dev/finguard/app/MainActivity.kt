package dev.finguard.app

import android.graphics.Color
import android.os.Bundle
import android.view.ViewGroup
import android.webkit.WebView
import androidx.activity.enableEdgeToEdge
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
  }

  // enableEdgeToEdge() (and, from Android 15 on this targetSdk, the platform itself)
  // lets the WebView draw under the status and navigation bars, so page content
  // renders behind the status bar icons unless something feeds the inset back in.
  // Shrink the WebView's own bounds by the system bar insets here (see the margin
  // comment below), and make the WebView background transparent so the dark
  // android:windowBackground shows through the resulting gap instead of the
  // WebView's own default white background.
  override fun onWebViewCreate(webView: WebView) {
    super.onWebViewCreate(webView)
    // The generated RustWebView leaves useWideViewPort at its Android default (false),
    // which makes the WebView ignore the page's viewport meta tag and lay out at a
    // fixed wide virtual viewport instead of the screen width. Enable it so the page
    // renders at device width, and loadWithOverviewMode so that width is applied on
    // first load instead of the initial zoomed-out overview.
    webView.settings.useWideViewPort = true
    webView.settings.loadWithOverviewMode = true
    webView.setBackgroundColor(Color.TRANSPARENT)
    ViewCompat.setOnApplyWindowInsetsListener(webView) { view, windowInsets ->
      val bars = windowInsets.getInsets(
        WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout()
      )
      // View.setPadding() does not work here: WebView lays out and paints its
      // Chromium content at the view's full measured bounds regardless of its own
      // padding, so page content still draws under the status bar even though the
      // padding is set. Layout margins do work, because they change the bounds
      // the parent hands the WebView to measure and lay out into in the first
      // place, so the Chromium content itself renders into the shrunk area.
      val params = view.layoutParams as? ViewGroup.MarginLayoutParams
      if (params != null) {
        params.setMargins(bars.left, bars.top, bars.right, bars.bottom)
        view.layoutParams = params
      }
      WindowInsetsCompat.CONSUMED
    }
  }
}
