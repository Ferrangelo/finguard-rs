package dev.finguard.app

import android.os.Bundle
import android.webkit.WebView
import androidx.activity.enableEdgeToEdge

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
  }

  override fun onWebViewCreate(webView: WebView) {
    super.onWebViewCreate(webView)
    // The generated RustWebView leaves useWideViewPort at its Android default (false),
    // which makes the WebView ignore the page's viewport meta tag and lay out at a
    // fixed wide virtual viewport instead of the screen width. Enable it so the page
    // renders at device width, and loadWithOverviewMode so that width is applied on
    // first load instead of the initial zoomed-out overview.
    webView.settings.useWideViewPort = true
    webView.settings.loadWithOverviewMode = true
  }
}
